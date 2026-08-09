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
    drivers::{
        ahci::AhciError,
        block_io::{self, BlockBuffer, WriteFlags},
    },
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
pub const PAGE_SIZE: usize = 4096;
/// Sectors per 4 KiB page.
const SECTORS_PER_PAGE: u16 = 8;
/// A dirty page is only written back if it has been dirty for at least this
/// long. Matches Linux's `dirty_expire_centisecs` concept (Linux default 30s;
/// we use 5s since our metadata volume is small). Forced flushes (sync/fsync)
/// ignore this and flush everything immediately.
const DIRTY_EXPIRE_MS: u64 = 5_000;
/// How many times a writer drains the cache waiting for a shard slot before
/// giving up and taking a detached page.
const WRITE_PAGE_ATTEMPTS: usize = 3;

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

/// Issue a single-page read via the block-io trait.
fn read_frame(device_id: u64, page_block_idx: u64, frame: PhysFrame) -> Result<(), AhciError> {
    let lba = page_block_idx * SECTORS_PER_PAGE as u64;
    let buf = frame_slice(frame);
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    let h = dev.submit_read(
        lba,
        SECTORS_PER_PAGE as u32,
        BlockBuffer::Slice {
            ptr: buf.as_mut_ptr(),
            len: PAGE_SIZE,
        },
    )?;
    h.wait()?;
    Ok(())
}

/// Issue a single-page write via the block-io trait.
fn write_frame(device_id: u64, page_block_idx: u64, frame: PhysFrame) -> Result<(), AhciError> {
    let lba = page_block_idx * SECTORS_PER_PAGE as u64;
    let buf = frame_slice(frame);
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    let h = dev.submit_write(
        lba,
        SECTORS_PER_PAGE as u32,
        BlockBuffer::Slice {
            ptr: buf.as_mut_ptr(),
            len: PAGE_SIZE,
        },
        WriteFlags::NONE,
    )?;
    h.wait()?;
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
    /// already exists (race), return that existing page and surface `new_page`
    /// in `to_drop` (its Drop frees the frame — the caller drops `to_drop`
    /// AFTER releasing the shard lock so `frame_allocator()` is acquired with
    /// no BPC rank on the stack). Otherwise evict an LRU entry (if any, same
    /// deferred-drop treatment) and insert.
    /// Insert `new_page`, or resolve a race with a concurrent filler.
    ///
    /// Returns the page to use and whether it is actually in the cache. A
    /// `false` means the shard was full of pinned or dirty pages and the page
    /// is *detached*: writeback will never find it, so a writer holding one
    /// must push the bytes to disk itself rather than marking it dirty.
    fn insert_or_resolve_race(
        &self,
        shard: &mut ShardInner,
        key: Key,
        new_page: Arc<CachedBlockPage>,
        to_drop: &mut Vec<Arc<CachedBlockPage>>,
    ) -> (Arc<CachedBlockPage>, bool) {
        if let Some(existing) = shard.lru.get(&key) {
            // Another thread filled this page while we were doing I/O.
            // `new_page` has to be dropped outside the shard lock (its Drop
            // calls frame_allocator, rank 90 < BPC.shard 110).
            let resolved = Arc::clone(existing);
            to_drop.push(new_page);
            return (resolved, true);
        }

        // Evict LRU entries that are neither pinned nor dirty until we make room.
        while shard.lru.len() >= SHARD_CAPACITY {
            let evict_key = match shard.lru.peek_lru() {
                Some((&k, page)) if page.pin_count() == 0 && !page.is_dirty() => k,
                _ => {
                    // All or the LRU candidate is pinned/dirty -- kick writeback
                    // before returning detached so the dirty pages drain soon.
                    self.kick_writeback();
                    let n = self
                        .stats
                        .detached_fallbacks
                        .fetch_add(1, Ordering::Relaxed)
                        + 1;
                    // Log the first occurrence + every 1000th after that. A
                    // busy fsync over a 256-entry shard can generate hundreds
                    // of fallbacks per second; per-call log saturates the
                    // UART (115200 baud) and throttles the whole kernel via
                    // the serial lock. The counter in /proc/bpc_stats is the
                    // authoritative source.
                    if n == 1 || n.is_multiple_of(1000) {
                        log!(
                            "block_page_cache: detached fallback #{} for key ({}, {})",
                            n,
                            key.0,
                            key.1
                        );
                    }
                    return (new_page, false);
                }
            };
            // pop the evicted Arc into `to_drop`; it drops outside the shard
            // scope so `CachedBlockPage::drop`'s deallocate_frame (rank 90)
            // doesn't inverse-acquire under BPC.shard (rank 110).
            if let Some(evicted) = shard.lru.pop(&evict_key) {
                self.stats.evictions.fetch_add(1, Ordering::Relaxed);
                to_drop.push(evicted);
            }
        }

        shard.lru.put(key, Arc::clone(&new_page));
        (new_page, true)
    }

    // ---- Public API ------------------------------------------------------

    /// Fetch a single page, filling from disk on miss.
    pub fn read_page(
        &self,
        device_id: u64,
        page_block_idx: u64,
    ) -> Result<BlockPageGuard, AhciError> {
        Ok(self.read_page_tracked(device_id, page_block_idx)?.0)
    }

    /// As [`read_page_tracked`], but waits for cache space rather than
    /// accepting a detached page.
    ///
    /// A detached page has to be written straight to its home location, which
    /// puts it on the disk *before* the journal has committed it: a replay
    /// after a crash would then overwrite newer data with the journal's older
    /// copy. Draining the cache and retrying keeps every write inside the
    /// ordering the journal guarantees. The detached page is still returned as
    /// a last resort, because failing the write outright is worse.
    ///
    /// [`read_page_tracked`]: Self::read_page_tracked
    fn read_page_for_write(
        &self,
        device_id: u64,
        page_block_idx: u64,
    ) -> Result<(BlockPageGuard, bool), AhciError> {
        for attempt in 0..WRITE_PAGE_ATTEMPTS {
            let got = self.read_page_tracked(device_id, page_block_idx)?;
            if got.1 || attempt + 1 == WRITE_PAGE_ATTEMPTS {
                return Ok(got);
            }
            // Drop our pin first: it is one of the things keeping the shard
            // from making room, then let writeback run. Kicking rather than
            // flushing inline: a writer reaches here holding filesystem locks
            // that rank *above* the shard lock, so doing the flush on this
            // thread would invert the order.
            drop(got);
            self.kick_writeback();
            crate::thread::scheduler::thread_yield();
        }
        unreachable!("loop returns on its last iteration")
    }

    /// As [`read_page`], and also reports whether the page is in the cache.
    /// Writers need to know: see [`insert_or_resolve_race`].
    ///
    /// [`read_page`]: Self::read_page
    /// [`insert_or_resolve_race`]: Self::insert_or_resolve_race
    fn read_page_tracked(
        &self,
        device_id: u64,
        page_block_idx: u64,
    ) -> Result<(BlockPageGuard, bool), AhciError> {
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
                return Ok((BlockPageGuard::new(Arc::clone(page)), true));
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
        let mut to_drop: Vec<Arc<CachedBlockPage>> = Vec::with_capacity(2);
        let (resolved, cached) = {
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
            self.insert_or_resolve_race(&mut shard, key, new_page, &mut to_drop)
        };
        drop(to_drop);
        Ok((BlockPageGuard::new(resolved), cached))
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

        // Hand off to the trait's submit_read_batch. AHCI overrides this with
        // its NCQ batch path for hardware-level parallelism; USB falls back to
        // the default serial-submit loop transparently.
        let dev = match block_io::lookup(device_id) {
            Some(d) => d,
            None => {
                for f in frames.iter().flatten() {
                    unsafe { frame_allocator().deallocate_frame(*f) };
                }
                return Err(AhciError::InvalidDevice);
            }
        };
        let reqs: Vec<(u64, u32, BlockBuffer)> = miss_indices
            .iter()
            .enumerate()
            .map(|(fi, &mi)| {
                let lba = (start_page + mi as u64) * SECTORS_PER_PAGE as u64;
                let buf = frame_slice(frames[fi].unwrap());
                (
                    lba,
                    SECTORS_PER_PAGE as u32,
                    BlockBuffer::Slice {
                        ptr: buf.as_mut_ptr(),
                        len: PAGE_SIZE,
                    },
                )
            })
            .collect();
        let handles = match dev.submit_read_batch(reqs) {
            Ok(h) => h,
            Err(e) => {
                for f in frames.iter().flatten() {
                    unsafe { frame_allocator().deallocate_frame(*f) };
                }
                return Err(e.into());
            }
        };
        let mut batch_err: Option<AhciError> = None;
        for h in &handles {
            if let Err(e) = h.wait() {
                batch_err.get_or_insert_with(|| e.into());
            }
        }
        if let Some(e) = batch_err {
            for f in frames.iter().flatten() {
                unsafe { frame_allocator().deallocate_frame(*f) };
            }
            return Err(e);
        }

        // Insert pages into cache, resolving any races.
        let mut to_drop: Vec<Arc<CachedBlockPage>> = Vec::new();
        for (fi, &mi) in miss_indices.iter().enumerate() {
            let key = (device_id, start_page + mi as u64);
            let si = shard_index(key);
            let new_page = Arc::new(CachedBlockPage::new(key, frames[fi].unwrap()));
            let (resolved, _cached) = {
                let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
                self.insert_or_resolve_race(&mut shard, key, new_page, &mut to_drop)
            };
            guards[mi] = Some(BlockPageGuard::new(resolved));
        }
        drop(to_drop);

        Ok(guards.into_iter().map(|g| g.unwrap()).collect())
    }

    /// Tell the device's journal that `key` has reached its home location.
    ///
    /// Every path that writes an enrolled block out must call this. A block
    /// written by any other route leaves its entry in the checkpoint tracker,
    /// the journal tail never advances past it, and the ring wedges once it
    /// fills: commits then fail forever with no way to reclaim space.
    fn note_checkpointed(&self, key: Key) {
        let journals = ranked_lock!(RANK_BPC_JOURNALS, "BPC.journals", self.journals);
        if let Some(j) = journals.get(&key.0) {
            if let Some(seq) = j.enrolled_seq(key.0, key.1) {
                j.note_checkpointed(key.0, key.1, seq);
            }
        }
    }

    /// Record a freshly written page.
    ///
    /// A cached page is marked dirty and left to writeback. A detached page is
    /// invisible to writeback, so its bytes go to the device now; dropping it
    /// otherwise loses the write silently.
    fn publish_write(
        &self,
        key: Key,
        page: &CachedBlockPage,
        cached: bool,
    ) -> Result<(), AhciError> {
        if !cached {
            write_frame(key.0, key.1, page.frame)?;
            self.note_checkpointed(key);
            return Ok(());
        }

        let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[shard_index(key)]);
        if shard.dirty.insert(key) {
            self.stats.dirty_pages.fetch_add(1, Ordering::Relaxed);
        }
        let shard_dirty_len = shard.dirty.len();
        drop(shard);
        if shard_dirty_len > SHARD_CAPACITY / 4 {
            self.kick_writeback();
        }
        Ok(())
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
        let (guard, cached) = {
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
            if let Some(page) = shard.lru.get(&key) {
                (BlockPageGuard::new(Arc::clone(page)), true)
            } else {
                drop(shard);
                // Allocate fresh frame (no read needed -- full overwrite).
                let frame = frame_allocator()
                    .allocate_frame()
                    .ok_or(AhciError::IoError)?;
                let new_page = Arc::new(CachedBlockPage::new(key, frame));
                let mut to_drop: Vec<Arc<CachedBlockPage>> = Vec::with_capacity(2);
                let (resolved, cached) = {
                    let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[si]);
                    self.insert_or_resolve_race(&mut shard, key, new_page, &mut to_drop)
                };
                drop(to_drop);
                (BlockPageGuard::new(resolved), cached)
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
            if cached {
                guard.mark_dirty();
            }
        }

        self.publish_write(key, &guard, cached)
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

        // Pin the page (fills from disk if not cached).
        let (guard, cached) = self.read_page_for_write(device_id, page_block_idx)?;

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
            if cached {
                guard.mark_dirty();
            }
        }

        self.publish_write(key, &guard, cached)
    }

    /// Read-modify-write an arbitrary byte range on a device, spanning as many
    /// pages as it covers.
    ///
    /// Raw device access from userspace lands here. Every page is read before
    /// it is patched, so a write that covers part of a page leaves the rest
    /// intact; nothing is rounded outward and no neighbouring sector is lost.
    pub fn write_bytes(
        &self,
        device_id: u64,
        byte_offset: u64,
        data: &[u8],
    ) -> Result<(), AhciError> {
        let mut written = 0usize;
        while written < data.len() {
            let pos = byte_offset + written as u64;
            let page_block_idx = pos / PAGE_SIZE as u64;
            let offset_in_page = (pos % PAGE_SIZE as u64) as usize;
            let chunk = (PAGE_SIZE - offset_in_page).min(data.len() - written);
            let key = (device_id, page_block_idx);

            let (guard, cached) = self.read_page_for_write(device_id, page_block_idx)?;
            {
                let _wl = ranked_lock!(
                    RANK_PAGE_WRITE_LOCK,
                    "BPC.page.write_lock",
                    guard.write_lock
                );
                // SAFETY: we hold write_lock.
                let dest = unsafe { guard.as_mut_slice() };
                dest[offset_in_page..offset_in_page + chunk]
                    .copy_from_slice(&data[written..written + chunk]);
                if cached {
                    guard.mark_dirty();
                }
            }

            self.publish_write(key, &guard, cached)?;
            written += chunk;
        }
        Ok(())
    }

    /// Copy an arbitrary byte range out of a device, spanning as many pages as
    /// it covers.
    pub fn read_bytes(
        &self,
        device_id: u64,
        byte_offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, AhciError> {
        let mut out = alloc::vec![0u8; len];
        let mut done = 0usize;
        while done < len {
            let pos = byte_offset + done as u64;
            let page_block_idx = pos / PAGE_SIZE as u64;
            let offset_in_page = (pos % PAGE_SIZE as u64) as usize;
            let chunk = (PAGE_SIZE - offset_in_page).min(len - done);

            let guard = self.read_page(device_id, page_block_idx)?;
            out[done..done + chunk]
                .copy_from_slice(&guard.as_slice()[offset_in_page..offset_in_page + chunk]);

            done += chunk;
        }
        Ok(out)
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

        // submit_flush is a no-op on devices without a hardware write cache
        // (USB MSC today), and issues FLUSH CACHE EXT on AHCI.
        let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
        let h = dev.submit_flush()?;
        h.wait()?;
        Ok(())
    }

    /// True when this block is dirty in the cache, i.e. its home copy is
    /// older than what the cache holds.
    pub fn is_dirty(&self, device_id: u64, page_block_idx: u64) -> bool {
        let key = (device_id, page_block_idx);
        let shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[shard_index(key)]);
        shard.dirty.contains(&key)
    }

    /// Drop cached pages for `count` pages starting at `first_page`.
    ///
    /// For callers that write those blocks straight to the device, bypassing
    /// this cache: whatever is cached is older than what they just wrote, so
    /// it is discarded rather than written back. Leaving it would let a later
    /// writeback pass put the stale copy back on top of the new data.
    pub fn invalidate_pages(&self, device_id: u64, first_page: u64, count: u64) {
        let mut to_drop: Vec<Arc<CachedBlockPage>> = Vec::new();
        for page in first_page..first_page + count {
            let key = (device_id, page);
            let mut shard =
                ranked_lock!(RANK_BPC_SHARD, "BPC.shard", self.shards[shard_index(key)]);
            if let Some(evicted) = shard.lru.pop(&key) {
                to_drop.push(evicted);
            }
            if shard.dirty.remove(&key) {
                self.stats.dirty_pages.fetch_sub(1, Ordering::Relaxed);
            }
            drop(shard);

            // The caller has just put this block at its home location, so it
            // is checkpointed whether or not it was ever flushed from here.
            // Skipping this leaves the entry in the journal's tracker, and a
            // tracker entry that never clears pins the tail: the ring fills,
            // commits start failing, and every write after that crawls.
            self.note_checkpointed(key);
        }
        // Frames free in CachedBlockPage::drop, which takes the frame
        // allocator (rank 910); drop outside the shard lock (110).
        drop(to_drop);
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
            // Collect evicted Arcs and drop them AFTER releasing the shard
            // lock; CachedBlockPage::drop acquires frame_allocator (rank 90)
            // which is below BPC.shard (rank 110).
            let mut to_drop: Vec<Arc<CachedBlockPage>> = Vec::new();
            let mut shard = ranked_lock!(RANK_BPC_SHARD, "BPC.shard", shard);
            let keys: Vec<Key> = shard
                .lru
                .iter()
                .filter(|(k, _)| k.0 == device_id)
                .map(|(&k, _)| k)
                .collect();
            for key in &keys {
                if let Some(evicted) = shard.lru.pop(key) {
                    to_drop.push(evicted);
                }
            }
            // Anything still dirty was missed by the flush above; write it now
            // rather than dropping the page. Evicting dirty data silently is
            // indistinguishable from filesystem corruption later.
            let still_dirty: Vec<Arc<CachedBlockPage>> = to_drop
                .iter()
                .filter(|p| p.is_dirty())
                .map(Arc::clone)
                .collect();
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
            drop(shard);

            for page in still_dirty {
                if let Err(e) = write_frame(page.key.0, page.key.1, page.frame) {
                    log!(
                        "block_page_cache: writing back dirty page ({}, {}) during invalidate failed: {:?}",
                        page.key.0,
                        page.key.1,
                        e
                    );
                } else {
                    page.clear_dirty();
                    self.note_checkpointed(page.key);
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
