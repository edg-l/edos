// Block-granularity page cache for metadata I/O.
#![allow(dead_code)]
//
// Write semantics: WRITE-BACK. Writes update the in-memory page and mark it
// dirty; a background writeback kthread (fs::writeback) periodically flushes
// dirty pages to disk.
//
// Crash-consistency risk: unflushed dirty pages are lost on power failure.
// A journal (step 3 of the storage roadmap) will eliminate this risk by
// providing ordered, atomic metadata updates before they are written through.
//
// Dirty tracking uses a BTreeSet<Key> per shard. The writeback thread uses a
// snapshot-then-conditional-remove protocol to avoid losing dirtied pages that
// were re-dirtied while a flush was in flight.
//
// Flush sequencing uses two AtomicU64 counters:
//   flush_requested  -- bumped by kick_writeback() and the 5 s periodic tick.
//   flush_completed  -- set to the req value the thread started with after
//                       each pass; monotonically non-decreasing.
// sync_all() increments flush_requested, wakes the writeback thread, then
// blocks on sync_done_wq until flush_completed >= its request number.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lru::LruCache;
use spin::Once;
use x86_64::structures::paging::{FrameAllocator, PhysFrame};

use crate::{
    debug::lock_order::{
        RANK_BPC_JOURNALS, RANK_BPC_SHARD, RANK_JOURNAL_TRACKER, RANK_PAGE_WRITE_LOCK,
    },
    drivers::ahci::{AhciError, direct},
    log,
    memory::{frame_allocator::frame_allocator, get_virt_addr_from_phys_offset},
    ranked_lock,
    thread::{mutex::BlockingMutex, waitqueue::WaitQueue},
};

// ---------------------------------------------------------------------------
// Tunable constants
// ---------------------------------------------------------------------------

/// Number of LRU shards. Must be a power of two (used for hash masking).
const NUM_SHARDS: usize = 8;
/// LRU entries per shard.
const SHARD_CAPACITY: usize = 256;
/// Page size in bytes.
const PAGE_SIZE: usize = 4096;
/// Sectors per 4 KiB page.
const SECTORS_PER_PAGE: u16 = 8;
/// Device IDs >= this value are USB storage devices.
const USB_DEVICE_ID_BASE: u64 = 1000;
/// A dirty page is only written back if it has been dirty for at least this
/// long. Matches Linux's `dirty_expire_centisecs` concept (Linux default 30s;
/// we use 5s since our metadata volume is small). Forced flushes (sync/fsync)
/// ignore this and flush everything immediately.
const DIRTY_EXPIRE_MS: u64 = 5_000;

// ---------------------------------------------------------------------------
// CachedBlockPage
// ---------------------------------------------------------------------------

pub struct CachedBlockPage {
    /// (device_id, page_block_idx)
    pub key: (u64, u64),
    pub frame: PhysFrame,
    pub dirty: AtomicBool,
    /// HPET tick when this page was first dirtied (0 = not dirty).
    /// Used for dirty_expire: writeback skips recently-dirtied pages.
    pub dirty_since_tick: AtomicU64,
    pub pin_count: AtomicU32,
    /// Serializes partial and full-page writers on the same page.
    pub write_lock: BlockingMutex<()>,
}

impl CachedBlockPage {
    fn new(key: (u64, u64), frame: PhysFrame) -> Self {
        Self {
            key,
            frame,
            dirty: AtomicBool::new(false),
            dirty_since_tick: AtomicU64::new(0),
            pin_count: AtomicU32::new(0),
            write_lock: BlockingMutex::new(()),
        }
    }

    /// Virtual address of this page's frame via HHDM.
    pub fn virt_addr(&self) -> *mut u8 {
        get_virt_addr_from_phys_offset(self.frame.start_address()).as_mut_ptr()
    }

    /// View page contents as a byte slice.
    ///
    /// # Safety
    /// No concurrent mutable access may exist; hold the write_lock if mutating.
    pub unsafe fn as_slice(&self) -> &[u8; PAGE_SIZE] {
        unsafe { &*(self.virt_addr() as *const [u8; PAGE_SIZE]) }
    }

    /// View page contents as a mutable byte slice.
    ///
    /// # Safety
    /// Caller must hold write_lock to exclude other writers.
    pub unsafe fn as_mut_slice(&self) -> &mut [u8; PAGE_SIZE] {
        unsafe { &mut *(self.virt_addr() as *mut [u8; PAGE_SIZE]) }
    }

    pub fn mark_dirty(&self) {
        // Only set the timestamp on the first dirty (not re-dirty).
        if !self.dirty.swap(true, Ordering::AcqRel) {
            let tick = crate::timer::Instant::now().tick();
            self.dirty_since_tick.store(tick, Ordering::Release);
        }
    }

    pub fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Release);
        self.dirty_since_tick.store(0, Ordering::Release);
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// Returns true if this page has been dirty for at least `DIRTY_EXPIRE_MS`.
    pub fn is_expired(&self) -> bool {
        let tick = self.dirty_since_tick.load(Ordering::Acquire);
        if tick == 0 {
            return false;
        }
        let now = crate::timer::Instant::now();
        let since = crate::timer::Instant::from_tick(tick);
        now.duration_since(since).as_millis() as u64 >= DIRTY_EXPIRE_MS
    }

    pub fn pin(&self) {
        self.pin_count.fetch_add(1, Ordering::AcqRel);
    }

    pub fn unpin(&self) {
        self.pin_count.fetch_sub(1, Ordering::Release);
    }

    pub fn pin_count(&self) -> u32 {
        self.pin_count.load(Ordering::Acquire)
    }
}

impl Drop for CachedBlockPage {
    fn drop(&mut self) {
        debug_assert_eq!(
            self.pin_count(),
            0,
            "CachedBlockPage dropped with pin_count={} (guard leaked?)",
            self.pin_count()
        );
        unsafe { frame_allocator().deallocate_frame(self.frame) };
    }
}

// ---------------------------------------------------------------------------
// BlockPageGuard -- RAII pin guard
// ---------------------------------------------------------------------------

pub struct BlockPageGuard {
    page: Arc<CachedBlockPage>,
}

impl BlockPageGuard {
    fn new(page: Arc<CachedBlockPage>) -> Self {
        page.pin();
        Self { page }
    }

    /// Read the page contents.
    pub fn as_slice(&self) -> &[u8; PAGE_SIZE] {
        // SAFETY: pin_count > 0, no writer can free the frame; write_lock
        // serializes writers so this shared read is safe.
        unsafe { self.page.as_slice() }
    }

    /// Mutable view -- caller must hold page.write_lock externally.
    ///
    /// # Safety
    /// Caller must hold the page's write_lock.
    pub unsafe fn as_mut_slice(&self) -> &mut [u8; PAGE_SIZE] {
        unsafe { self.page.as_mut_slice() }
    }

    /// Return a clone of the underlying `Arc<CachedBlockPage>` so callers can
    /// enroll the page in a journal transaction.
    pub fn page_arc(&self) -> Arc<CachedBlockPage> {
        Arc::clone(&self.page)
    }
}

impl core::ops::Deref for BlockPageGuard {
    type Target = CachedBlockPage;
    fn deref(&self) -> &Self::Target {
        &self.page
    }
}

impl Drop for BlockPageGuard {
    fn drop(&mut self) {
        self.page.unpin();
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

pub struct StatsSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub detached_fallbacks: u64,
    pub dirty_pages: u64,
    pub writeback_runs: u64,
    pub writeback_bytes: u64,
    pub sync_calls: u64,
    pub flush_requested: u64,
    pub flush_completed: u64,
}

pub(super) struct Stats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub evictions: AtomicU64,
    pub detached_fallbacks: AtomicU64,
    pub dirty_pages: AtomicU64,
    pub writeback_runs: AtomicU64,
    pub writeback_bytes: AtomicU64,
    pub sync_calls: AtomicU64,
}

impl Stats {
    const fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            detached_fallbacks: AtomicU64::new(0),
            dirty_pages: AtomicU64::new(0),
            writeback_runs: AtomicU64::new(0),
            writeback_bytes: AtomicU64::new(0),
            sync_calls: AtomicU64::new(0),
        }
    }

    fn snapshot(&self, flush_requested: u64, flush_completed: u64) -> StatsSnapshot {
        StatsSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            detached_fallbacks: self.detached_fallbacks.load(Ordering::Relaxed),
            dirty_pages: self.dirty_pages.load(Ordering::Relaxed),
            writeback_runs: self.writeback_runs.load(Ordering::Relaxed),
            writeback_bytes: self.writeback_bytes.load(Ordering::Relaxed),
            sync_calls: self.sync_calls.load(Ordering::Relaxed),
            flush_requested,
            flush_completed,
        }
    }
}

// ---------------------------------------------------------------------------
// Shard -- LRU + dirty set under one lock
// ---------------------------------------------------------------------------

struct ShardInner {
    lru: LruCache<Key, Arc<CachedBlockPage>>,
    dirty: BTreeSet<Key>,
}

impl ShardInner {
    fn new() -> Self {
        Self {
            lru: LruCache::new(core::num::NonZero::new(SHARD_CAPACITY).unwrap()),
            dirty: BTreeSet::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// BlockPageCache
// ---------------------------------------------------------------------------

type Key = (u64, u64);
type Shard = BlockingMutex<ShardInner>;

pub struct BlockPageCache {
    shards: [Shard; NUM_SHARDS],
    pub(super) stats: Stats,
    /// Monotonically increasing; each kick or periodic tick increments this.
    pub flush_requested: AtomicU64,
    /// Set to the `flush_requested` value at the start of the last completed
    /// writeback pass. `flush_completed >= N` means request N is done.
    pub flush_completed: AtomicU64,
    /// Writeback thread waits here between kicks and the 5 s periodic tick.
    pub writeback_wq: WaitQueue,
    /// sync_all() waiters block here until their request is complete.
    pub sync_done_wq: WaitQueue,
    /// Per-device journal handles registered at mount time.
    /// Wired in Phase 5; populated by `register_device`.
    journals: BlockingMutex<BTreeMap<u64, Arc<crate::fs::journal::Journal>>>,
}

static BLOCK_PAGE_CACHE: Once<BlockPageCache> = Once::new();
static CACHE_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn shard_index(key: Key) -> usize {
    // FNV-inspired mix -- keeps shard selection fast and lock-free.
    let h = key
        .0
        .wrapping_mul(0x517cc1b727220a95)
        .wrapping_add(key.1.wrapping_mul(0x6c62272e07bb0142));
    (h as usize) & (NUM_SHARDS - 1)
}

/// Build a HHDM slice pointing at a frame's contents.
fn frame_slice(frame: PhysFrame) -> &'static mut [u8] {
    let ptr = get_virt_addr_from_phys_offset(frame.start_address()).as_mut_ptr::<u8>();
    // SAFETY: frame is a valid 4 KiB physical frame mapped via HHDM.
    unsafe { core::slice::from_raw_parts_mut(ptr, PAGE_SIZE) }
}

fn is_usb(device_id: u64) -> bool {
    device_id >= USB_DEVICE_ID_BASE
}

/// Issue a single-page read from the appropriate backend.
fn read_frame(device_id: u64, page_block_idx: u64, frame: PhysFrame) -> Result<(), AhciError> {
    let lba = page_block_idx * SECTORS_PER_PAGE as u64;
    let buf = frame_slice(frame);
    if is_usb(device_id) {
        let data = crate::drivers::usb::block_api::usb_read_sectors(
            lba,
            SECTORS_PER_PAGE,
            vec![0u8; PAGE_SIZE],
        )
        .map_err(|_| AhciError::IoError)?;
        let n = data.len().min(PAGE_SIZE);
        buf[..n].copy_from_slice(&data[..n]);
    } else {
        direct::read_sectors(device_id, lba, SECTORS_PER_PAGE, buf)?;
    }
    Ok(())
}

/// Issue a single-page write to the appropriate backend.
fn write_frame(device_id: u64, page_block_idx: u64, frame: PhysFrame) -> Result<(), AhciError> {
    let lba = page_block_idx * SECTORS_PER_PAGE as u64;
    let buf = frame_slice(frame);
    if is_usb(device_id) {
        crate::drivers::usb::block_api::usb_write_sectors(lba, SECTORS_PER_PAGE, buf.to_vec())
            .map_err(|_| AhciError::IoError)?;
    } else {
        direct::write_sectors(device_id, lba, buf, SECTORS_PER_PAGE)?;
    }
    Ok(())
}

impl BlockPageCache {
    fn new() -> Self {
        // Array init requires Copy/Default, so build via a closure workaround.
        let shards = core::array::from_fn(|_| BlockingMutex::new(ShardInner::new()));
        Self {
            shards,
            stats: Stats::new(),
            flush_requested: AtomicU64::new(0),
            flush_completed: AtomicU64::new(0),
            writeback_wq: WaitQueue::new(),
            sync_done_wq: WaitQueue::new(),
            journals: BlockingMutex::new(BTreeMap::new()),
        }
    }

    /// Initialize the global cache. Call once during boot.
    pub fn init() {
        BLOCK_PAGE_CACHE.call_once(BlockPageCache::new);
        CACHE_INITIALIZED.store(true, Ordering::Release);
    }

    /// Access the global instance. Lazy-initialized on first use if init()
    /// has not been called.
    pub fn global() -> &'static BlockPageCache {
        let c = BLOCK_PAGE_CACHE.call_once(BlockPageCache::new);
        CACHE_INITIALIZED.store(true, Ordering::Release);
        c
    }

    /// Returns true if the cache has been initialized.
    pub fn initialized() -> bool {
        CACHE_INITIALIZED.load(Ordering::Acquire)
    }

    // ---- Journal registry -----------------------------------------------

    /// Associate a journal with a block device. Called at EFS mount time
    /// (Phase 5 wires this in; the method compiles but is not called in Phase 3).
    pub fn register_device(&self, device_id: u64, journal: Arc<crate::fs::journal::Journal>) {
        ranked_lock!(RANK_BPC_JOURNALS, "BPC.journals", self.journals).insert(device_id, journal);
    }

    /// Look up the journal for a device, if one has been registered.
    pub fn journal_for_device(&self, device_id: u64) -> Option<Arc<crate::fs::journal::Journal>> {
        ranked_lock!(RANK_BPC_JOURNALS, "BPC.journals", self.journals)
            .get(&device_id)
            .cloned()
    }

    /// Return all registered journals (for the committer kthread).
    pub fn all_journals(&self) -> Vec<Arc<crate::fs::journal::Journal>> {
        ranked_lock!(RANK_BPC_JOURNALS, "BPC.journals", self.journals)
            .values()
            .cloned()
            .collect()
    }

    // ---- Internal helpers ------------------------------------------------

    /// Try to insert `new_page` into the shard. If an entry for the key
    /// already exists (race), return that existing page and free `frame`.
    /// Otherwise evict an LRU entry if the shard is full and insert.
    fn insert_or_resolve_race(
        &self,
        shard: &mut ShardInner,
        key: Key,
        new_page: Arc<CachedBlockPage>,
    ) -> Arc<CachedBlockPage> {
        if let Some(existing) = shard.lru.get(&key) {
            // Another thread filled this page while we were doing I/O.
            // Our `new_page` Arc drops at return; its Drop frees the frame.
            return Arc::clone(existing);
        }

        // Evict LRU entries that are neither pinned nor dirty until we make room.
        while shard.lru.len() >= SHARD_CAPACITY {
            let evict_key = match shard.lru.peek_lru() {
                Some((&k, page)) if page.pin_count() == 0 && !page.is_dirty() => k,
                _ => {
                    // All or the LRU candidate is pinned/dirty -- kick writeback
                    // before returning detached so the dirty pages drain soon.
                    self.kick_writeback();
                    self.stats
                        .detached_fallbacks
                        .fetch_add(1, Ordering::Relaxed);
                    log!(
                        "block_page_cache: shard full of pinned pages, detached fallback for key ({}, {})",
                        key.0,
                        key.1
                    );
                    return new_page;
                }
            };
            // pop removes from LRU; the evicted Arc drops (if refcount hits 0
            // here, Drop frees the frame; otherwise a holder will free it later).
            if shard.lru.pop(&evict_key).is_some() {
                self.stats.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }

        shard.lru.put(key, Arc::clone(&new_page));
        new_page
    }

    // ---- Public API ------------------------------------------------------

    /// Fetch a single page, filling from disk on miss.
    pub fn read_page(
        &self,
        device_id: u64,
        page_block_idx: u64,
    ) -> Result<BlockPageGuard, AhciError> {
        assert!(
            Self::initialized(),
            "BlockPageCache::read_page called before init()"
        );

        let key = (device_id, page_block_idx);
        let si = shard_index(key);

        // Fast path: already cached.
        {
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
            if let Some(page) = shard.lru.get(&key) {
                self.stats.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(BlockPageGuard::new(Arc::clone(page)));
            }
        }
        self.stats.misses.fetch_add(1, Ordering::Relaxed);

        // Slow path: allocate frame, read from disk (no shard lock held).
        let frame = frame_allocator()
            .allocate_frame()
            .ok_or(AhciError::IoError)?;
        debug_assert_eq!(
            frame.start_address().as_u64() & (PAGE_SIZE as u64 - 1),
            0,
            "frame_allocator returned unaligned frame"
        );
        if let Err(e) = read_frame(device_id, page_block_idx, frame) {
            unsafe { frame_allocator().deallocate_frame(frame) };
            return Err(e);
        }

        let new_page = Arc::new(CachedBlockPage::new(key, frame));
        let resolved = {
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
            self.insert_or_resolve_race(&mut shard, key, new_page)
        };
        Ok(BlockPageGuard::new(resolved))
    }

    /// Fetch multiple consecutive pages, issuing bulk I/O for misses.
    pub fn read_pages(
        &self,
        device_id: u64,
        start_page: u64,
        count: usize,
    ) -> Result<Vec<BlockPageGuard>, AhciError> {
        assert!(
            Self::initialized(),
            "BlockPageCache::read_pages called before init()"
        );

        if count == 0 {
            return Ok(Vec::new());
        }

        let mut guards: Vec<Option<BlockPageGuard>> = (0..count).map(|_| None).collect();
        let mut miss_indices: Vec<usize> = Vec::new();

        // Check cache for each page.
        for i in 0..count {
            let key = (device_id, start_page + i as u64);
            let si = shard_index(key);
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
            if let Some(page) = shard.lru.get(&key) {
                self.stats.hits.fetch_add(1, Ordering::Relaxed);
                guards[i] = Some(BlockPageGuard::new(Arc::clone(page)));
            } else {
                miss_indices.push(i);
            }
        }

        if miss_indices.is_empty() {
            return Ok(guards.into_iter().map(|g| g.unwrap()).collect());
        }

        self.stats
            .misses
            .fetch_add(miss_indices.len() as u64, Ordering::Relaxed);

        // Allocate frames for all misses.
        let mut frames: Vec<Option<PhysFrame>> = vec![None; miss_indices.len()];
        for (fi, &mi) in miss_indices.iter().enumerate() {
            match frame_allocator().allocate_frame() {
                Some(f) => {
                    debug_assert_eq!(
                        f.start_address().as_u64() & (PAGE_SIZE as u64 - 1),
                        0,
                        "frame_allocator returned unaligned frame"
                    );
                    frames[fi] = Some(f);
                }
                None => {
                    // Free already-allocated frames and bail.
                    for prev in frames[..fi].iter().flatten() {
                        unsafe { frame_allocator().deallocate_frame(*prev) };
                    }
                    return Err(AhciError::IoError);
                }
            }
            let _ = mi; // suppress lint
        }

        if is_usb(device_id) {
            // USB: sequential reads into each frame.
            for (fi, &mi) in miss_indices.iter().enumerate() {
                let frame = frames[fi].unwrap();
                let page_idx = start_page + mi as u64;
                if let Err(e) = read_frame(device_id, page_idx, frame) {
                    for f in frames.iter().flatten() {
                        unsafe { frame_allocator().deallocate_frame(*f) };
                    }
                    return Err(e);
                }
            }
        } else {
            // AHCI: group contiguous miss indices into runs for batch I/O.
            // Each run gets a slice into its frame(s).
            // For simplicity, issue each miss as an individual direct read
            // -- the AHCI driver handles concurrent NCQ internally.
            // A future optimisation can coalesce truly-contiguous runs.
            let mut batch: Vec<(u64, u16, &mut [u8])> = miss_indices
                .iter()
                .enumerate()
                .map(|(fi, &mi)| {
                    let lba = (start_page + mi as u64) * SECTORS_PER_PAGE as u64;
                    let buf = frame_slice(frames[fi].unwrap());
                    (lba, SECTORS_PER_PAGE, buf)
                })
                .collect();

            if let Err(e) = direct::read_sectors_batch(device_id, &mut batch) {
                for f in frames.iter().flatten() {
                    unsafe { frame_allocator().deallocate_frame(*f) };
                }
                return Err(e);
            }
        }

        // Insert pages into cache, resolving any races.
        for (fi, &mi) in miss_indices.iter().enumerate() {
            let key = (device_id, start_page + mi as u64);
            let si = shard_index(key);
            let new_page = Arc::new(CachedBlockPage::new(key, frames[fi].unwrap()));
            let resolved = {
                let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
                self.insert_or_resolve_race(&mut shard, key, new_page)
            };
            guards[mi] = Some(BlockPageGuard::new(resolved));
        }

        Ok(guards.into_iter().map(|g| g.unwrap()).collect())
    }

    /// Write a full page. Write-back: marks the page dirty for the background
    /// writeback thread to flush to disk asynchronously.
    pub fn write_page(
        &self,
        device_id: u64,
        page_block_idx: u64,
        data: &[u8; PAGE_SIZE],
    ) -> Result<(), AhciError> {
        assert!(
            Self::initialized(),
            "BlockPageCache::write_page called before init()"
        );

        let key = (device_id, page_block_idx);
        let si = shard_index(key);

        // Get or create the cached page.
        let guard = {
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
            if let Some(page) = shard.lru.get(&key) {
                BlockPageGuard::new(Arc::clone(page))
            } else {
                drop(shard);
                // Allocate fresh frame (no read needed -- full overwrite).
                let frame = frame_allocator()
                    .allocate_frame()
                    .ok_or(AhciError::IoError)?;
                let new_page = Arc::new(CachedBlockPage::new(key, frame));
                let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
                let resolved = self.insert_or_resolve_race(&mut shard, key, new_page);
                BlockPageGuard::new(resolved)
            }
        };

        // Copy data into the frame and mark dirty (write-back).
        {
            let _wl = ranked_lock!(
                RANK_PAGE_WRITE_LOCK,
                "BPC.page.write_lock",
                guard.write_lock
            );
            // SAFETY: we hold write_lock.
            let dest = unsafe { guard.as_mut_slice() };
            dest.copy_from_slice(data);
            guard.mark_dirty();
        }

        // Insert into dirty set; kick writeback if shard pressure threshold reached.
        {
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
            let newly_inserted = shard.dirty.insert(key);
            if newly_inserted {
                self.stats.dirty_pages.fetch_add(1, Ordering::Relaxed);
            }
            let shard_dirty_len = shard.dirty.len();
            drop(shard);
            if shard_dirty_len > SHARD_CAPACITY / 4 {
                self.kick_writeback();
            }
        }

        Ok(())
    }

    /// Read-modify-write: update a sub-sector range within a page then mark dirty.
    ///
    /// `lba` is the absolute LBA on disk; `sectors` is how many 512 B sectors
    /// starting at `lba` are covered by `data`. The owning page is identified
    /// by aligning `lba` down to the nearest 8-sector boundary.
    pub fn write_partial_page(
        &self,
        device_id: u64,
        lba: u64,
        sectors: u16,
        data: &[u8],
    ) -> Result<(), AhciError> {
        assert!(
            Self::initialized(),
            "BlockPageCache::write_partial_page called before init()"
        );

        assert_eq!(
            data.len(),
            sectors as usize * 512,
            "write_partial_page: data length must equal sectors * 512"
        );

        let page_block_idx = lba / SECTORS_PER_PAGE as u64;
        let offset_in_page = ((lba % SECTORS_PER_PAGE as u64) * 512) as usize;
        let len = sectors as usize * 512;
        let key = (device_id, page_block_idx);
        let si = shard_index(key);

        // Pin the page (fills from disk if not cached).
        let guard = self.read_page(device_id, page_block_idx)?;

        // Serialize writers on this page.
        {
            let _wl = ranked_lock!(
                RANK_PAGE_WRITE_LOCK,
                "BPC.page.write_lock",
                guard.write_lock
            );
            // SAFETY: we hold write_lock.
            let dest = unsafe { guard.as_mut_slice() };
            dest[offset_in_page..offset_in_page + len].copy_from_slice(data);
            guard.mark_dirty();
        }

        // Insert into dirty set; kick writeback if shard pressure threshold reached.
        {
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
            let newly_inserted = shard.dirty.insert(key);
            if newly_inserted {
                self.stats.dirty_pages.fetch_add(1, Ordering::Relaxed);
            }
            let shard_dirty_len = shard.dirty.len();
            drop(shard);
            if shard_dirty_len > SHARD_CAPACITY / 4 {
                self.kick_writeback();
            }
        }

        Ok(())
    }

    /// Signal the writeback thread that there is work to do.
    pub fn kick_writeback(&self) {
        self.flush_requested.fetch_add(1, Ordering::Release);
        self.writeback_wq.wake_all();
    }

    /// Flush all dirty pages for all devices in one pass.
    ///
    /// Uses snapshot-then-conditional-remove to avoid losing pages that are
    /// re-dirtied while we are writing them. Returns the number of bytes written.
    /// Flush dirty pages to disk. If `force` is false, only pages that have
    /// been dirty for at least `DIRTY_EXPIRE_MS` are flushed (periodic writeback).
    /// If `force` is true, all dirty pages are flushed immediately (sync/fsync).
    pub fn flush_dirty_once(&self, force: bool) -> Result<u64, AhciError> {
        let mut bytes_written: u64 = 0;

        for (si, shard_lock) in self.shards.iter().enumerate() {
            // Snapshot the dirty set without holding the lock during I/O.
            let keys_snapshot: Vec<Key> = {
                let shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", shard_lock);
                shard.dirty.iter().copied().collect()
            };

            for key in keys_snapshot {
                // Re-fetch the page under the lock (it may have been evicted).
                let page = {
                    let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", shard_lock);
                    shard.lru.get(&key).cloned()
                };
                let Some(page) = page else {
                    // Evicted between snapshot and now; remove from dirty set.
                    let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", shard_lock);
                    if shard.dirty.remove(&key) {
                        self.stats.dirty_pages.fetch_sub(1, Ordering::Relaxed);
                    }
                    continue;
                };

                // Journal gating: skip pages that have not yet been committed.
                // Lock order: BPC.journals (120) -> Journal.checkpoint_tracker (130).
                let journal_seq_for_page: Option<u64> = {
                    let journals = ranked_lock!(RANK_BPC_JOURNALS, "BPC.journals", self.journals);
                    if let Some(j) = journals.get(&key.0) {
                        let tracker = ranked_lock!(
                            RANK_JOURNAL_TRACKER,
                            "Journal.checkpoint_tracker",
                            j.checkpoint_tracker
                        );
                        tracker.get(&key).copied()
                    } else {
                        None // No journal for this device — always flushable.
                    }
                };

                if let Some(enrolled_seq) = journal_seq_for_page {
                    let committed = {
                        let journals =
                            ranked_lock!(RANK_BPC_JOURNALS, "BPC.journals", self.journals);
                        journals
                            .get(&key.0)
                            .map(|j| j.committed_seq())
                            .unwrap_or(u64::MAX)
                    };
                    if enrolled_seq > committed {
                        // Not yet committed; leave in dirty set and skip.
                        continue;
                    }
                }

                // Dirty-expire: skip recently-dirtied pages on periodic writeback.
                // Forced flushes (sync/fsync) bypass this to guarantee durability.
                if !force && !page.is_expired() {
                    continue;
                }

                // Acquire write_lock to serialize against concurrent writers.
                let _wg =
                    ranked_lock!(RANK_PAGE_WRITE_LOCK, "BPC.page.write_lock", page.write_lock);
                if !page.is_dirty() {
                    continue;
                }

                write_frame(page.key.0, page.key.1, page.frame)?;
                page.clear_dirty();
                bytes_written += PAGE_SIZE as u64;
                drop(_wg);

                // Notify the journal that this block has been checkpointed.
                // Lock order: BPC.journals (120) -> Journal.checkpoint_tracker (130) inside
                // note_checkpointed.
                if let Some(enrolled_seq) = journal_seq_for_page {
                    let journals = ranked_lock!(RANK_BPC_JOURNALS, "BPC.journals", self.journals);
                    if let Some(j) = journals.get(&key.0) {
                        j.note_checkpointed(key.0, key.1, enrolled_seq);
                    }
                }

                // Only remove from dirty set if the page was not re-dirtied
                // by a concurrent writer between clear_dirty and this check.
                {
                    let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", shard_lock);
                    if !page.is_dirty() {
                        if shard.dirty.remove(&key) {
                            self.stats.dirty_pages.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                }

                let _ = si; // suppress lint
            }
        }

        Ok(bytes_written)
    }

    /// Wait until the writeback thread has completed at least request `req`.
    fn wait_for_flush(&self, req: u64) {
        self.sync_done_wq
            .wait_until(|| self.flush_completed.load(Ordering::Acquire) >= req);
    }

    /// Flush all dirty pages synchronously. Kicks the writeback thread and
    /// blocks until the pass that covers this call has completed.
    pub fn sync_all(&self) {
        self.stats.sync_calls.fetch_add(1, Ordering::Relaxed);
        let req = self.flush_requested.fetch_add(1, Ordering::Release) + 1;
        self.writeback_wq.wake_all();
        self.wait_for_flush(req);
    }

    /// Flush all dirty pages for a specific device, then issue an AHCI cache
    /// flush command for non-USB devices.
    ///
    /// Uses the writeback sequencing protocol: kicks the thread, waits for the
    /// pass to complete, then issues the hardware flush command.
    pub fn flush_device(&self, device_id: u64) -> Result<(), AhciError> {
        let req = self.flush_requested.fetch_add(1, Ordering::Release) + 1;
        self.writeback_wq.wake_all();
        self.wait_for_flush(req);

        if !is_usb(device_id) {
            direct::flush_cache(device_id)?;
        }
        Ok(())
    }

    /// Remove all cached pages for a device (e.g., on unmount).
    ///
    /// Flushes dirty pages first (log-and-continue on error), then invalidates
    /// all cached pages for the device and cleans up the dirty set.
    pub fn invalidate_device(&self, device_id: u64) {
        if let Err(e) = self.flush_device(device_id) {
            log!(
                "block_page_cache: flush_device failed during invalidate for device {}: {:?}",
                device_id,
                e
            );
        }

        for shard in &self.shards {
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", shard);
            let keys: Vec<Key> = shard
                .lru
                .iter()
                .filter(|(k, _)| k.0 == device_id)
                .map(|(&k, _)| k)
                .collect();
            for key in &keys {
                // Drop Arc; Drop on CachedBlockPage frees the frame when the
                // last reference goes away.
                shard.lru.pop(key);
            }
            // Remove device entries from the dirty set.
            let dirty_keys: Vec<Key> = shard
                .dirty
                .iter()
                .copied()
                .filter(|k| k.0 == device_id)
                .collect();
            for key in dirty_keys {
                if shard.dirty.remove(&key) {
                    self.stats.dirty_pages.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot(
            self.flush_requested.load(Ordering::Relaxed),
            self.flush_completed.load(Ordering::Relaxed),
        )
    }
}
