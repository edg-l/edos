//! Per-inode page cache.
//!
//! LOCK ORDERING: vfs inode lock < page_cache pages lock < mm-mapper lock.
//! Truncate-invalidate acquires in this order; faults drop mm-mapper lock
//! before taking the page_cache lock (see fault.rs).
//!
//! Write policy divergence:
//!   - `write(2)` paths (`page_cache_write` in vfs.rs) are write-through:
//!     data is written into the cache page and immediately flushed to disk.
//!   - MAP_SHARED mappings are write-back: stores go directly into the cache
//!     frame (the kernel never sees individual stores), the page is marked
//!     dirty on the first fault, and data is flushed by msync(MS_SYNC),
//!     munmap, fsync, or the periodic writeback kthread.
//!   This does NOT break the "file data bypasses BlockPageCache" invariant:
//!   `PageCacheOps::flush_page` routes through direct AHCI, not BlockPageCache.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use x86_64::structures::paging::{FrameAllocator, PhysFrame};

use crate::{
    fs::Error,
    memory::{frame_allocator::frame_allocator, get_virt_addr_from_phys_offset},
    thread::mutex::BlockingMutex,
};

const PAGE_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// CachedPage
// ---------------------------------------------------------------------------

pub struct CachedPage {
    frame: PhysFrame,
    dirty: AtomicBool,
    pin_count: AtomicU32,
}

impl CachedPage {
    fn new(frame: PhysFrame) -> Self {
        Self {
            frame,
            dirty: AtomicBool::new(false),
            pin_count: AtomicU32::new(0),
        }
    }

    pub fn virt_addr(&self) -> *mut u8 {
        get_virt_addr_from_phys_offset(self.frame.start_address()).as_mut_ptr()
    }

    /// # Safety
    /// Caller must ensure no mutable aliasing exists for this frame.
    pub unsafe fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.virt_addr(), PAGE_SIZE) }
    }

    /// # Safety
    /// Caller must ensure exclusive access to this frame.
    pub unsafe fn as_slice_mut(&self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.virt_addr(), PAGE_SIZE) }
    }

    pub fn pin(&self) {
        self.pin_count.fetch_add(1, Ordering::AcqRel);
    }

    pub fn unpin(&self) {
        self.pin_count.fetch_sub(1, Ordering::AcqRel);
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

    pub fn frame(&self) -> PhysFrame {
        self.frame
    }
}

// ---------------------------------------------------------------------------
// PageGuard -- RAII pin guard
// ---------------------------------------------------------------------------

pub struct PageGuard {
    page: Arc<CachedPage>,
}

impl PageGuard {
    fn new(page: Arc<CachedPage>) -> Self {
        page.pin();
        Self { page }
    }
}

impl core::ops::Deref for PageGuard {
    type Target = CachedPage;
    fn deref(&self) -> &Self::Target {
        &self.page
    }
}

impl PageGuard {
    /// Clone the underlying `Arc<CachedPage>` without dropping the guard.
    /// Callers that want to keep the page alive after the guard drops must
    /// call `page.pin()` on the returned Arc themselves.
    pub fn arc(&self) -> Arc<CachedPage> {
        Arc::clone(&self.page)
    }
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        self.page.unpin();
    }
}

// ---------------------------------------------------------------------------
// Per-inode page map (stored on VfsInode)
// ---------------------------------------------------------------------------

/// Per-inode page cache. Each file gets its own map + lock, so different files
/// have zero contention (matching Linux's per-address_space design).
pub struct InodePages {
    pub pages: BlockingMutex<BTreeMap<u64, Arc<CachedPage>>>,
    dirty_keys: BlockingMutex<Vec<u64>>,
}

impl InodePages {
    pub fn new() -> Self {
        Self {
            pages: BlockingMutex::new(BTreeMap::new()),
            dirty_keys: BlockingMutex::new(Vec::new()),
        }
    }

    /// Get a cached page or fill it via `fill_fn` (which does disk I/O).
    /// The per-inode lock is NOT held during I/O.
    pub fn get_or_fill(
        &self,
        page_index: u64,
        fill_fn: impl FnOnce(&mut [u8]) -> Result<(), Error>,
    ) -> Result<PageGuard, Error> {
        // Fast path: already cached.
        {
            let map = self.pages.lock();
            if let Some(page) = map.get(&page_index) {
                return Ok(PageGuard::new(Arc::clone(page)));
            }
        }

        // Slow path: allocate frame, fill from disk, insert.
        let frame = frame_allocator().allocate_frame().ok_or(Error::IoError)?;

        let slice: &mut [u8] = unsafe {
            let ptr = get_virt_addr_from_phys_offset(frame.start_address()).as_mut_ptr();
            core::slice::from_raw_parts_mut(ptr, PAGE_SIZE)
        };

        if let Err(e) = fill_fn(slice) {
            unsafe { frame_allocator().deallocate_frame(frame) };
            return Err(e);
        }

        let page = Arc::new(CachedPage::new(frame));
        let guard = PageGuard::new(Arc::clone(&page));

        let mut map = self.pages.lock();
        if let Some(existing) = map.get(&page_index) {
            // Another thread raced us. Use theirs, free ours.
            let g = PageGuard::new(Arc::clone(existing));
            drop(map);
            unsafe { frame_allocator().deallocate_frame(frame) };
            return Ok(g);
        }
        map.insert(page_index, page);
        Ok(guard)
    }

    /// Mark a page as dirty.
    pub fn mark_dirty(&self, page_index: u64) {
        if let Some(page) = self.pages.lock().get(&page_index) {
            page.mark_dirty();
        }
        let mut dk = self.dirty_keys.lock();
        if !dk.contains(&page_index) {
            dk.push(page_index);
        }
    }

    /// Flush all dirty pages via `flush_fn`. Lock not held during I/O.
    pub fn flush_dirty(
        &self,
        mut flush_fn: impl FnMut(u64, &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let dirty: Vec<(u64, Arc<CachedPage>)> = {
            let dk = self.dirty_keys.lock();
            let map = self.pages.lock();
            dk.iter()
                .filter_map(|&idx| map.get(&idx).map(|p| (idx, Arc::clone(p))))
                .collect()
        };

        for (idx, page) in &dirty {
            page.pin();
            let result = flush_fn(*idx, unsafe { page.as_slice() });
            page.unpin();
            result?;
            page.clear_dirty();
        }

        let mut dk = self.dirty_keys.lock();
        for (idx, _) in &dirty {
            dk.retain(|k| k != idx);
        }
        Ok(())
    }

    /// Remove pages at page_index >= from_page. For truncate.
    pub fn invalidate_from(&self, from_page: u64) {
        let evicted: Vec<Arc<CachedPage>> = {
            let mut map = self.pages.lock();
            let keys: Vec<u64> = map.keys().filter(|&&k| k >= from_page).copied().collect();
            let mut pages = Vec::new();
            for k in keys {
                if let Some(p) = map.remove(&k) {
                    pages.push(p);
                }
            }
            pages
        };
        self.dirty_keys.lock().retain(|k| *k < from_page);
        for page in evicted {
            if Arc::strong_count(&page) == 1 {
                unsafe { frame_allocator().deallocate_frame(page.frame()) };
            }
        }
    }

    /// Remove all cached pages.
    pub fn invalidate_all(&self) {
        self.invalidate_from(0);
    }
}

// ---------------------------------------------------------------------------
// PageCacheOps trait
// ---------------------------------------------------------------------------

/// Operations a filesystem driver must implement to back the page cache.
pub trait PageCacheOps {
    /// Read a page of file data from disk into `buf`.
    /// Returns the number of valid bytes (rest should be zeroed for partial last page).
    fn fill_page(&self, ino: u64, page_index: u64, buf: &mut [u8]) -> Result<usize, Error>;

    /// Bulk read: read `count` bytes starting at `offset` in one operation.
    /// Default returns Unsupported; callers fall back to per-page fill_page.
    fn fill_pages_bulk(
        &self,
        _ino: u64,
        _offset: usize,
        _count: usize,
    ) -> Result<alloc::vec::Vec<u8>, Error> {
        Err(Error::Unsupported)
    }

    /// Write a page of file data to disk from `buf`.
    fn flush_page(
        &self,
        ino: u64,
        page_index: u64,
        buf: &[u8],
        valid_bytes: usize,
    ) -> Result<(), Error>;

    /// Update the on-disk file size. **Grow-only**: implementations MUST be
    /// no-ops when `new_size <= current file size`. Explicit shrinking is the
    /// responsibility of `FileSystem::truncate`. The page cache calls `update_size`
    /// after a write to record the new EOF, but never to shrink.
    fn update_size(&self, ino: u64, new_size: u64) -> Result<(), Error>;
}
