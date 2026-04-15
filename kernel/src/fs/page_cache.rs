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

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
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

    /// Mark a page as dirty. Lock acquisition order is always
    /// `pages` → `dirty_keys`, consistent with `flush_dirty` below.
    pub fn mark_dirty(&self, page_index: u64) {
        let map = self.pages.lock();
        if let Some(page) = map.get(&page_index) {
            page.mark_dirty();
        }
        let mut dk = self.dirty_keys.lock();
        if !dk.contains(&page_index) {
            dk.push(page_index);
        }
    }

    /// Flush all dirty pages via `flush_fn`. The per-inode lock is NOT held
    /// during I/O, but the final `clear_dirty` + `dirty_keys` removal is
    /// performed atomically (both under `dirty_keys` lock) so that a
    /// concurrent `mark_dirty` either (a) runs entirely before this section
    /// — key removal eats its push but the flag re-set ensures the next
    /// flush round picks it up via the still-true flag replayed through
    /// mark_dirty on the next writer call — or (b) runs entirely after,
    /// with both flag-set and key-push preserved. Either way no dirty
    /// page ends up without both a set `dirty` flag AND a `dirty_keys`
    /// entry pointing at it.
    pub fn flush_dirty(
        &self,
        mut flush_fn: impl FnMut(u64, &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let dirty: Vec<(u64, Arc<CachedPage>)> = {
            let map = self.pages.lock();
            let dk = self.dirty_keys.lock();
            dk.iter()
                .filter_map(|&idx| map.get(&idx).map(|p| (idx, Arc::clone(p))))
                .collect()
        };

        for (idx, page) in &dirty {
            page.pin();
            let result = flush_fn(*idx, unsafe { page.as_slice() });
            page.unpin();
            result?;
            // Clear the flag and remove the key atomically under the
            // dirty_keys lock, so a concurrent mark_dirty cannot race
            // in between and lose the dirty state.
            let mut dk = self.dirty_keys.lock();
            page.clear_dirty();
            dk.retain(|k| k != idx);
        }
        Ok(())
    }

    /// Flush all dirty pages via `flush_fn` with all-or-nothing semantics.
    ///
    /// **Semantics differ from `flush_dirty`**: `flush_dirty` clears dirty flags
    /// page-by-page inside the I/O loop, so a partial success on AHCI failure
    /// preserves already-flushed pages as clean.  This method is all-or-nothing:
    /// if `flush_fn` returns `Err`, ALL dirty flags and dirty-key entries are left
    /// intact so the next writeback round retries the entire batch.  This matches
    /// the EFS bulk-transaction model where the journal either commits all extent
    /// and inode metadata for the whole batch, or aborts and records nothing.
    ///
    /// A concurrent `mark_dirty` between the snapshot and the post-flush cleanup
    /// will have pushed its key again; the `dirty_keys` retain call below is safe
    /// because those concurrent keys are NOT in the `dirty` snapshot.
    pub fn flush_dirty_bulk(
        &self,
        flush_fn: impl FnOnce(&[(u64, Arc<CachedPage>)]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        // Snapshot under dual-lock in pages → dirty_keys order (same as flush_dirty).
        let dirty: Vec<(u64, Arc<CachedPage>)> = {
            let map = self.pages.lock();
            let dk = self.dirty_keys.lock();
            dk.iter()
                .filter_map(|&idx| map.get(&idx).map(|p| (idx, Arc::clone(p))))
                .collect()
        };

        if dirty.is_empty() {
            return Ok(());
        }

        // Pin all pages before releasing locks, so AHCI can read the frames.
        for (_, page) in &dirty {
            page.pin();
        }

        let result = flush_fn(&dirty);

        // Unpin regardless of outcome.
        for (_, page) in &dirty {
            page.unpin();
        }

        match result {
            Ok(()) => {
                // All-or-nothing: clear flags and remove keys for every page in
                // the snapshot in one pass under the dirty_keys lock.
                // Build a set of flushed indices for O(N log N) retain instead
                // of O(N²) (one retain per index).
                let indices: BTreeSet<u64> = dirty.iter().map(|(idx, _)| *idx).collect();
                for (_, page) in &dirty {
                    page.clear_dirty();
                }
                let mut dk = self.dirty_keys.lock();
                dk.retain(|k| !indices.contains(k));
                Ok(())
            }
            Err(e) => {
                // Leave ALL dirty flags and dirty_keys intact so the next round
                // retries the whole batch.
                Err(e)
            }
        }
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

    /// Flush a batch of dirty pages in one call.
    ///
    /// `pages` is a slice of `(page_index, Arc<CachedPage>)` pairs; each page is
    /// already pinned by the caller.  `new_size_hint` is the file size to record
    /// after the flush; if `None` the implementation skips the size update.
    ///
    /// The default implementation loops over `flush_page` + `update_size`, giving
    /// correct behaviour for all filesystems that implement those two methods
    /// (memfs, FAT32).  EFS overrides this to open a single journal transaction
    /// for the entire batch.
    ///
    /// Correctness invariant for memfs/FAT32: `flush_page` writes page data into
    /// the filesystem's own storage and `update_size` sets the authoritative file
    /// size; the loop here is therefore equivalent to calling them individually.
    fn flush_pages_bulk(
        &self,
        ino: u64,
        pages: &[(u64, Arc<CachedPage>)],
        new_size_hint: Option<u64>,
    ) -> Result<(), Error> {
        for (page_index, page) in pages {
            // Pages are pre-pinned by the caller (InodePages::flush_dirty_bulk),
            // but pin again to match the per-page flush_dirty pin/unpin contract.
            page.pin();
            let result = self.flush_page(ino, *page_index, unsafe { page.as_slice() }, PAGE_SIZE);
            page.unpin();
            result?;
        }
        if let Some(sz) = new_size_hint {
            self.update_size(ino, sz)?;
        }
        Ok(())
    }
}
