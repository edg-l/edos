// Block-granularity page cache for metadata I/O.
#![allow(dead_code)]
//
// Stores 4 KiB pages (one EFS block = one page) keyed by (device_id, page_block_idx).
// Uses 8 shards to reduce contention. Each shard holds an LRU map with capacity
// SHARD_CAPACITY (256), giving a total ceiling of 2048 pages = 8 MiB.
//
// Write semantics are write-through: every write flushes synchronously to the
// device before returning.

use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lru::LruCache;
use spin::Once;
use x86_64::structures::paging::{FrameAllocator, PhysFrame};

use crate::{
    drivers::ahci::{AhciError, direct},
    log,
    memory::{frame_allocator::frame_allocator, get_virt_addr_from_phys_offset},
    thread::mutex::BlockingMutex,
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

// ---------------------------------------------------------------------------
// CachedBlockPage
// ---------------------------------------------------------------------------

pub struct CachedBlockPage {
    /// (device_id, page_block_idx)
    pub key: (u64, u64),
    pub frame: PhysFrame,
    pub dirty: AtomicBool,
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
        self.dirty.store(true, Ordering::Release);
    }

    pub fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Release);
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
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

    /// Mutable view — caller must hold page.write_lock externally.
    ///
    /// # Safety
    /// Caller must hold the page's write_lock.
    pub unsafe fn as_mut_slice(&self) -> &mut [u8; PAGE_SIZE] {
        unsafe { self.page.as_mut_slice() }
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
}

struct Stats {
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    detached_fallbacks: AtomicU64,
}

impl Stats {
    const fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            detached_fallbacks: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            detached_fallbacks: self.detached_fallbacks.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// BlockPageCache
// ---------------------------------------------------------------------------

type Key = (u64, u64);
type Shard = BlockingMutex<LruCache<Key, Arc<CachedBlockPage>>>;

pub struct BlockPageCache {
    shards: [Shard; NUM_SHARDS],
    stats: Stats,
}

static BLOCK_PAGE_CACHE: Once<BlockPageCache> = Once::new();
static CACHE_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn shard_index(key: Key) -> usize {
    // FNV-inspired mix — keeps shard selection fast and lock-free.
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
        let shards = core::array::from_fn(|_| {
            BlockingMutex::new(LruCache::new(
                core::num::NonZero::new(SHARD_CAPACITY).unwrap(),
            ))
        });
        Self {
            shards,
            stats: Stats::new(),
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

    // ---- Internal helpers ------------------------------------------------

    /// Try to insert `new_page` into the shard. If an entry for the key
    /// already exists (race), return that existing page and free `frame`.
    /// Otherwise evict an LRU entry if the shard is full and insert.
    fn insert_or_resolve_race(
        &self,
        shard: &mut LruCache<Key, Arc<CachedBlockPage>>,
        key: Key,
        new_page: Arc<CachedBlockPage>,
    ) -> Arc<CachedBlockPage> {
        if let Some(existing) = shard.get(&key) {
            // Another thread filled this page while we were doing I/O.
            // Our `new_page` Arc drops at return; its Drop frees the frame.
            return Arc::clone(existing);
        }

        // Evict LRU entries that are neither pinned nor dirty until we make room.
        while shard.len() >= SHARD_CAPACITY {
            let evict_key = match shard.peek_lru() {
                Some((&k, page)) if page.pin_count() == 0 && !page.is_dirty() => k,
                _ => {
                    // All or the LRU candidate is pinned/dirty — return detached.
                    // When the caller drops the guard, the Arc's Drop frees the frame.
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
            if shard.pop(&evict_key).is_some() {
                self.stats.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }

        shard.put(key, Arc::clone(&new_page));
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
            let mut shard = self.shards[si].lock();
            if let Some(page) = shard.get(&key) {
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
            let mut shard = self.shards[si].lock();
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
            let mut shard = self.shards[si].lock();
            if let Some(page) = shard.get(&key) {
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
            // — the AHCI driver handles concurrent NCQ internally.
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
                let mut shard = self.shards[si].lock();
                self.insert_or_resolve_race(&mut shard, key, new_page)
            };
            guards[mi] = Some(BlockPageGuard::new(resolved));
        }

        Ok(guards.into_iter().map(|g| g.unwrap()).collect())
    }

    /// Write a full page. Acquires write_lock to serialize against partial writers.
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
            let mut shard = self.shards[si].lock();
            if let Some(page) = shard.get(&key) {
                BlockPageGuard::new(Arc::clone(page))
            } else {
                drop(shard);
                // Allocate fresh frame (no read needed — full overwrite).
                let frame = frame_allocator()
                    .allocate_frame()
                    .ok_or(AhciError::IoError)?;
                let new_page = Arc::new(CachedBlockPage::new(key, frame));
                let mut shard = self.shards[si].lock();
                let resolved = self.insert_or_resolve_race(&mut shard, key, new_page);
                BlockPageGuard::new(resolved)
            }
        };

        // Write-through: copy data then flush to disk.
        {
            let _wl = guard.write_lock.lock();
            // SAFETY: we hold write_lock.
            let dest = unsafe { guard.as_mut_slice() };
            dest.copy_from_slice(data);
            write_frame(device_id, page_block_idx, guard.frame)?;
            guard.clear_dirty();
        }

        Ok(())
    }

    /// Read-modify-write: update a sub-sector range within a page then flush.
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

        // Pin the page (fills from disk if not cached).
        let guard = self.read_page(device_id, page_block_idx)?;

        // Serialize writers on this page.
        let _wl = guard.write_lock.lock();
        {
            // SAFETY: we hold write_lock.
            let dest = unsafe { guard.as_mut_slice() };
            dest[offset_in_page..offset_in_page + len].copy_from_slice(data);
        }
        write_frame(device_id, page_block_idx, guard.frame)?;
        guard.clear_dirty();

        Ok(())
    }

    /// Flush all dirty pages for a device synchronously.
    pub fn flush_device(&self, device_id: u64) -> Result<(), AhciError> {
        // Collect dirty pages across all shards.
        let mut dirty: Vec<Arc<CachedBlockPage>> = Vec::new();
        for shard in &self.shards {
            let shard = shard.lock();
            for (_, page) in shard.iter() {
                if page.key.0 == device_id && page.is_dirty() {
                    dirty.push(Arc::clone(page));
                }
            }
        }

        for page in &dirty {
            let _wl = page.write_lock.lock();
            if page.is_dirty() {
                write_frame(page.key.0, page.key.1, page.frame)?;
                page.clear_dirty();
            }
        }
        Ok(())
    }

    /// Remove all cached pages for a device (e.g., on unmount).
    pub fn invalidate_device(&self, device_id: u64) {
        for shard in &self.shards {
            let mut shard = shard.lock();
            let keys: Vec<Key> = shard
                .iter()
                .filter(|(k, _)| k.0 == device_id)
                .map(|(&k, _)| k)
                .collect();
            for key in keys {
                // Drop Arc; Drop on CachedBlockPage frees the frame when the
                // last reference goes away.
                shard.pop(&key);
            }
        }
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }
}
