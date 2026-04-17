// Phase 1: new module; call sites are wired in Phase 2. Until then, suppress
// dead-code lints so `make check` stays clean.
#![allow(dead_code)]
//! Per-inode async-compatible page fill: `PageFillHandle` + `get_or_fill_async_sync`.
//!
//! # Overview
//!
//! `PageFillHandle` is the coordination object installed in
//! `InodePages::in_flight` while a fill is in progress. It carries:
//!
//! - `inode: Weak<VfsInode>` — for cancel-path cleanup.
//! - `page_idx: u64` + `len: u32` — the range `[page_idx, page_idx + len)`
//!   this handle covers. Single-page fills have `len = 1`; bulk fills have
//!   `len = N` and the same `Arc<PageFillHandle>` is stored at every index in
//!   the range.
//! - `waiters: WaitQueue` — concurrent readers park here until the publisher
//!   transitions the state to a terminal value.
//! - `state: AtomicU8` — `FILL_PENDING` → `FILL_SUCCESS | FILL_FAILED`.
//!
//! # Publish-terminal-state-before-remove invariant
//!
//! Both `finish_success` and `finish_failed` MUST be called BEFORE the
//! publisher removes the handle from `in_flight`. Removing first would open a
//! window where a waiter wakes, re-checks `in_flight` (empty), re-checks
//! `pages` (also empty on failure), then races to install a new handle before
//! `finish_failed` fires — and later receives the stale `wake_all`.
//!
//! Correct ordering (enforced by `get_or_fill_async_sync`):
//!
//! - Success: insert pages into `pages(40)` → `finish_success()` → remove from
//!   `in_flight(42)` → `owned_ops_remove`.
//! - Failure: `finish_failed()` → remove from `in_flight(42)` → `owned_ops_remove`.
//!
//! # Lock order
//!
//! `pages(40) → in_flight(42)` — the only nested InodePages edge; ascending,
//! valid. See `doc/invariants/lock-order.md` rank table.
//!
//! `fill_fn` runs with NO InodePages lock held, so AHCI (170-200) and BPC
//! (110) acquisitions inside `fill_fn` are fine from rank 42's perspective.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU8, Ordering};
use x86_64::structures::paging::FrameAllocator;

use crate::{
    debug::lock_order::{RANK_IN_FLIGHT, RANK_PAGES},
    fs::{
        Error,
        inode::VfsInode,
        page_cache::{CachedPage, PageGuard},
    },
    memory::{frame_allocator::frame_allocator, frame_drop::FrameDrop},
    ranked_lock,
    thread::{cancel::CancellableOp, scheduler::sched, waitqueue::WaitQueue},
};

// ---------------------------------------------------------------------------
// State constants
// ---------------------------------------------------------------------------

/// Fill is in progress; waiters should park.
pub const FILL_PENDING: u8 = 0;
/// Fill succeeded; `pages` contains the result.
pub const FILL_SUCCESS: u8 = 1;
/// Fill failed; next reader must retry.
pub const FILL_FAILED: u8 = 2;

// ---------------------------------------------------------------------------
// PageFillHandle
// ---------------------------------------------------------------------------

/// Coordination object for an in-progress page fill.
///
/// Installed in `InodePages::in_flight` for the duration of the fill. Concurrent
/// readers that find this handle in `in_flight` park on `waiters` until the
/// publisher transitions `state` to a terminal value (`FILL_SUCCESS` or
/// `FILL_FAILED`), then consult `pages` directly.
///
/// Implements `CancellableOp` so the issuing thread's death causes `cancel()`
/// to fire from the reaper, transitioning state to `FILL_FAILED`, removing the
/// handle from `in_flight`, and waking all parked readers. They observe
/// `FILL_FAILED`, re-check `pages` (miss), then retry as publishers.
///
/// # Drop contract
///
/// `Drop` is the default (no blocking). By the time the last Arc releases,
/// state is always terminal (`FILL_SUCCESS` or `FILL_FAILED`) and all waiters
/// have been woken. Safe to drop from any context including the reaper kthread.
pub struct PageFillHandle {
    /// Back-reference to the owning inode. Upgraded only in `cancel()` to
    /// clean up the `in_flight` entry.
    pub inode: Weak<VfsInode>,
    /// First page index covered by this handle.
    pub page_idx: u64,
    /// Number of pages covered: `[page_idx, page_idx + len)`.
    /// 1 for single-page fills; >1 for bulk fills.
    pub len: u32,
    /// Parked readers wake here when the fill transitions to a terminal state.
    pub waiters: WaitQueue,
    /// Fill state machine: `FILL_PENDING` → `FILL_SUCCESS | FILL_FAILED`.
    pub state: AtomicU8,
}

impl PageFillHandle {
    /// Construct a new handle in `FILL_PENDING` state.
    pub fn new(inode: Weak<VfsInode>, page_idx: u64, len: u32) -> Arc<Self> {
        Arc::new(Self {
            inode,
            page_idx,
            len,
            waiters: WaitQueue::new(),
            state: AtomicU8::new(FILL_PENDING),
        })
    }

    /// Mark the fill as succeeded and wake all parked readers.
    ///
    /// Caller MUST have already inserted all `Arc<CachedPage>`s into
    /// `inode.pages.pages` before calling this. See module-level invariant.
    pub fn finish_success(&self) {
        self.state.store(FILL_SUCCESS, Ordering::Release);
        self.waiters.wake_all();
    }

    /// Mark the fill as failed and wake all parked readers.
    ///
    /// Caller MUST call this BEFORE removing the handle from `in_flight`.
    /// See module-level invariant.
    pub fn finish_failed(&self) {
        self.state.store(FILL_FAILED, Ordering::Release);
        self.waiters.wake_all();
    }

    /// Load the current state with Acquire ordering.
    pub fn load_state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// CancellableOp impl
// ---------------------------------------------------------------------------

impl CancellableOp for PageFillHandle {
    /// Called from `Thread::free` (reaper kthread) when the issuing thread dies.
    ///
    /// Transitions `FILL_PENDING → FILL_FAILED` atomically, removes all covered
    /// entries from `in_flight` (guarded by pointer comparison to avoid stomping
    /// a successor handle), then wakes all parked readers.
    ///
    /// Must be non-blocking: uses `IrqSpinlock` (not `BlockingMutex`) for
    /// `in_flight`.
    fn cancel(&self) {
        // CAS Pending -> Failed. If we lose the CAS, another path already
        // finished (finish_success, finish_failed, or a prior cancel).
        let _ = self.state.compare_exchange(
            FILL_PENDING,
            FILL_FAILED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        // Remove our entries from in_flight if the inode is still alive.
        // Weak::upgrade returns None if the VfsInode (and its in_flight map)
        // has already been dropped — no cleanup needed; the map is gone.
        if let Some(inode) = self.inode.upgrade() {
            // Use raw-pointer comparison: we only have &self here, and we
            // cannot construct an Arc<PageFillHandle> without unsafe from_raw.
            // The ptr_eq semantics are identical: compare data pointers.
            in_flight_remove_by_ptr(
                &inode,
                self as *const PageFillHandle,
                self.page_idx,
                self.len,
            );
        }

        // Wake waiters after the spinlock is released so any newly-arriving
        // reader that sees no map entry becomes a fresh publisher; our
        // wake_all hits an empty queue (no-op) harmlessly.
        self.waiters.wake_all();
    }

    fn id(&self) -> (&'static str, u64) {
        ("fill", self.page_idx)
    }
}

// ---------------------------------------------------------------------------
// in_flight helpers (Tasks 1.8 and 1.9)
// ---------------------------------------------------------------------------

/// Remove all entries in `[handle.page_idx, handle.page_idx + handle.len)`
/// from `inode.pages.in_flight`, but only if the stored `Arc` is pointer-equal
/// to `handle`. This guards against stomping a successor handle installed after
/// our `finish_success` already removed us (or a concurrent cancel racing
/// `finish_failed`).
///
/// Used by the publisher cleanup path (Tasks 1.5-1.7) and by `CancellableOp::cancel`.
pub fn in_flight_remove_all(inode: &VfsInode, handle: &Arc<PageFillHandle>) {
    let mut inflight = inode
        .pages
        .in_flight
        .lock_ranked(RANK_IN_FLIGHT, "InodePages.in_flight (remove_all)");
    for idx in handle.page_idx..(handle.page_idx + handle.len as u64) {
        if let Some(existing) = inflight.get(&idx) {
            if Arc::ptr_eq(existing, handle) {
                inflight.remove(&idx);
            }
        }
    }
}

/// Raw-pointer variant used by `CancellableOp::cancel` where we only have `&self`
/// and cannot construct a free-standing `Arc` for `Arc::ptr_eq`.
fn in_flight_remove_by_ptr(inode: &VfsInode, ptr: *const PageFillHandle, page_idx: u64, len: u32) {
    let mut inflight = inode
        .pages
        .in_flight
        .lock_ranked(RANK_IN_FLIGHT, "InodePages.in_flight (cancel)");
    for idx in page_idx..(page_idx + len as u64) {
        if let Some(existing) = inflight.get(&idx) {
            if Arc::as_ptr(existing) == ptr {
                inflight.remove(&idx);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// inline_fill_no_handle (Task 1.7b)
// ---------------------------------------------------------------------------

/// Fill a single page inline without installing a `PageFillHandle`.
///
/// Used when `owned_ops_push` fails (registry full): there is no cancel hookup,
/// no WaitQueue, no concurrent readers waiting on a handle. If the issuer dies
/// mid-fill, `FrameDrop` on its stack returns the allocated frame automatically;
/// there are no orphaned waiters because no handle was installed.
pub fn inline_fill_no_handle(
    inode: &Arc<VfsInode>,
    page_idx: u64,
    fill_fn: impl FnOnce(&mut [u8]) -> Result<(), Error>,
) -> Result<PageGuard, Error> {
    let frame = frame_allocator().allocate_frame().ok_or(Error::IoError)?;
    let fd = FrameDrop::new(frame);

    let fill_result = {
        let slice = unsafe { fd.frame_slice_mut() };
        fill_fn(slice)
    };

    if let Err(e) = fill_result {
        // fd drops here, frame is returned.
        return Err(e);
    }

    let frame = fd.forget();
    let page = Arc::new(CachedPage::new(frame));
    let guard = PageGuard::new(Arc::clone(&page));

    let mut pages = ranked_lock!(RANK_PAGES, "InodePages.pages", inode.pages.pages);
    if let Some(existing) = pages.get(&page_idx) {
        // Race: another thread inserted the page while fill_fn was running.
        let g = PageGuard::new(Arc::clone(existing));
        drop(pages);
        drop(guard);
        drop(page);
        return Ok(g);
    }
    pages.insert(page_idx, page);
    Ok(guard)
}

// ---------------------------------------------------------------------------
// get_or_fill_async_sync (Tasks 1.5, 1.6, 1.7)
// ---------------------------------------------------------------------------

/// Get or fill a single page using the in-flight registry protocol.
///
/// This is the new entry point that will replace direct `InodePages::get_or_fill`
/// calls. Behavior is identical to the legacy path in Phase 1 (the backend is
/// still the synchronous `fill_fn`), but concurrent readers for the same
/// `page_idx` park on the publisher's `PageFillHandle` rather than issuing
/// redundant I/O.
///
/// # Reader state machine
///
/// 1. **Fast path**: if the page is already in `pages(40)`, return immediately.
/// 2. **Slow path**: if another thread's `PageFillHandle` is in `in_flight(42)`,
///    park on its `WaitQueue` until it transitions to a terminal state, then
///    re-check `pages`. On `FILL_SUCCESS`, return the page. On `FILL_FAILED`,
///    retry as publisher.
/// 3. **Publisher path**: install a new handle in `in_flight(42)`, push to
///    `owned_ops` (fallback to `inline_fill_no_handle` on overflow), allocate
///    a `FrameDrop`, run `fill_fn`, publish the page under `pages(40)`, call
///    `finish_success` / `finish_failed`, remove from `in_flight(42)`, remove
///    from `owned_ops`.
///
/// # Lock order
///
/// `pages(40) → in_flight(42)` on the publish-and-remove step. All other
/// lock acquisitions are single-step with no other ranked lock held.
///
/// # Cancel safety
///
/// The publisher registers its `PageFillHandle` in the issuing thread's
/// `owned_ops`. On issuer death, `cancel()` fires: CAS to Failed, remove from
/// `in_flight`, wake waiters. Waiters retry as publishers.
///
/// # Phase 1 note
///
/// This function is defined but NOT YET called by anything — Phase 2 migrates
/// the call sites. The legacy `InodePages::get_or_fill` continues to be used.
pub fn get_or_fill_async_sync(
    inode: &Arc<VfsInode>,
    page_idx: u64,
    fill_fn: impl FnOnce(&mut [u8]) -> Result<(), Error>,
) -> Result<PageGuard, Error> {
    // Fast path: already cached.
    {
        let pages = ranked_lock!(RANK_PAGES, "InodePages.pages", inode.pages.pages);
        if let Some(page) = pages.get(&page_idx) {
            return Ok(PageGuard::new(Arc::clone(page)));
        }
    }

    // Wrap fill_fn in an Option so we can move it once into the publisher path.
    let mut fill_fn_opt = Some(fill_fn);

    loop {
        // Check if another thread is already filling this page.
        let handle_opt: Option<Arc<PageFillHandle>> = {
            let inflight = inode
                .pages
                .in_flight
                .lock_ranked(RANK_IN_FLIGHT, "InodePages.in_flight (slow-check)");
            inflight.get(&page_idx).cloned()
        };

        if let Some(handle) = handle_opt {
            // Park until the fill reaches a terminal state.
            // Outer loop on real condition per CLAUDE.md "Park/wake protocol".
            handle
                .waiters
                .wait_until(|| handle.load_state() != FILL_PENDING);
            drop(handle);

            // Re-check pages. Success: page is there. Failed/evicted: retry.
            {
                let pages = ranked_lock!(RANK_PAGES, "InodePages.pages", inode.pages.pages);
                if let Some(page) = pages.get(&page_idx) {
                    return Ok(PageGuard::new(Arc::clone(page)));
                }
            }
            // Woke with FILL_FAILED or page was evicted between wake and re-check.
            // Loop back and try again as publisher.
            continue;
        }

        // Publisher path: no existing handle. Install one.
        let handle = PageFillHandle::new(Arc::downgrade(inode), page_idx, 1);

        // Double-check under in_flight lock: a concurrent publisher may have
        // installed a handle between our slow-check release and this acquire.
        let install_result: Result<(), Arc<PageFillHandle>> = {
            let mut inflight = inode
                .pages
                .in_flight
                .lock_ranked(RANK_IN_FLIGHT, "InodePages.in_flight (install)");
            if let Some(existing) = inflight.get(&page_idx) {
                Err(existing.clone())
            } else {
                inflight.insert(page_idx, Arc::clone(&handle));
                Ok(())
            }
        };

        if let Err(existing) = install_result {
            // Lost the race; park on the winner's handle.
            existing
                .waiters
                .wait_until(|| existing.load_state() != FILL_PENDING);
            drop(existing);
            // Re-check pages, then retry.
            {
                let pages = ranked_lock!(RANK_PAGES, "InodePages.pages", inode.pages.pages);
                if let Some(page) = pages.get(&page_idx) {
                    return Ok(PageGuard::new(Arc::clone(page)));
                }
            }
            continue;
        }

        // We installed the handle. Register with owned_ops for cancel hookup.
        let op: Arc<dyn CancellableOp> = Arc::clone(&handle) as Arc<dyn CancellableOp>;
        let push_ok = sched()
            .current_thread()
            .map(|t| t.owned_ops_push(op).is_ok())
            .unwrap_or(false);

        if !push_ok {
            // owned_ops full: publish terminal state BEFORE removing from
            // in_flight so any reader that cloned the handle in the window
            // between our install and this point observes Failed and exits
            // its park loop (re-checks pages, retries as publisher). If we
            // removed first, that reader would park on a handle that never
            // transitions terminal -> permanent hang.
            handle.finish_failed();
            in_flight_remove_all(inode, &handle);
            let fill_fn = fill_fn_opt
                .take()
                .expect("get_or_fill_async_sync: fill_fn already consumed");
            return inline_fill_no_handle(inode, page_idx, fill_fn);
        }

        // Allocate the frame. On allocation failure, clean up and return error.
        let frame = match frame_allocator().allocate_frame() {
            Some(f) => f,
            None => {
                handle.finish_failed();
                in_flight_remove_all(inode, &handle);
                sched().current_thread().map(|t| {
                    t.owned_ops_remove(Arc::as_ptr(&handle) as *const ());
                });
                return Err(Error::IoError);
            }
        };
        let fd = FrameDrop::new(frame);

        // Run fill_fn with no InodePages lock held.
        let fill_fn = fill_fn_opt
            .take()
            .expect("get_or_fill_async_sync: fill_fn already consumed");
        let fill_result = {
            let slice = unsafe { fd.frame_slice_mut() };
            fill_fn(slice)
        };

        match fill_result {
            Ok(()) => {
                let frame = fd.forget();
                let page = Arc::new(CachedPage::new(frame));
                let guard = PageGuard::new(Arc::clone(&page));

                // Publish under pages(40). Double-check defensively in case of
                // a logic bug allowing a concurrent publisher for the same page_idx.
                {
                    let mut pages = ranked_lock!(RANK_PAGES, "InodePages.pages", inode.pages.pages);
                    if let Some(existing) = pages.get(&page_idx) {
                        // Our frame was allocated unnecessarily.
                        let g = PageGuard::new(Arc::clone(existing));
                        drop(pages);
                        // Publish-terminal-state-before-remove: success branch.
                        handle.finish_success();
                        in_flight_remove_all(inode, &handle);
                        sched().current_thread().map(|t| {
                            t.owned_ops_remove(Arc::as_ptr(&handle) as *const ());
                        });
                        // Drop our guard and page; CachedPage::drop returns the frame.
                        drop(guard);
                        drop(page);
                        return Ok(g);
                    }
                    pages.insert(page_idx, page);
                }

                // Publish-terminal-state-before-remove invariant (success branch):
                // finish_success (store Success + wake_all) BEFORE in_flight remove.
                handle.finish_success();
                in_flight_remove_all(inode, &handle);
                sched().current_thread().map(|t| {
                    t.owned_ops_remove(Arc::as_ptr(&handle) as *const ());
                });
                return Ok(guard);
            }
            Err(e) => {
                // fd drops here, frame is returned to allocator via FrameDrop::drop.
                drop(fd);

                // Publish-terminal-state-before-remove invariant (failure branch):
                // finish_failed (store Failed + wake_all) BEFORE in_flight remove.
                handle.finish_failed();
                in_flight_remove_all(inode, &handle);
                sched().current_thread().map(|t| {
                    t.owned_ops_remove(Arc::as_ptr(&handle) as *const ());
                });
                return Err(e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// get_or_fill_bulk_async_sync (Task 2.2)
// ---------------------------------------------------------------------------

/// Populate `inode.pages` for the exclusive range `[start_page, start_page + page_count)`
/// using a single `bulk_fill_fn` invocation.
///
/// A single `PageFillHandle` with `len = page_count` is created and installed at
/// EVERY index in the range under one `in_flight(42)` critical section. Any
/// concurrent reader for any page in the range parks on this handle's `WaitQueue`.
///
/// # Behavior when a range index already has a handle
///
/// If ANY index in `[start_page, start_page + page_count)` already has an
/// in-flight handle (installed by a concurrent single-page reader), the entire
/// bulk publish is aborted. The caller parks on the FIRST such handle, then
/// retries the bulk install from the top. This is simpler than narrowing the
/// bulk range; it is safe because after parking the conflicting page will be
/// cached (success) or absent (retry naturally).
///
/// # owned_ops push failure
///
/// If the issuing thread's `owned_ops` registry is full, the bulk install is
/// abandoned entirely: the just-installed handle is removed, `finish_failed` is
/// called to wake any readers that snuck in, and `Err(Error::IoError)` is
/// returned so the caller can fall back to the per-page
/// `get_or_fill_async_sync` path.
///
/// # Lock order
///
/// Same edges as single-page path: `pages(40) → in_flight(42)` on
/// publish-and-remove; ascending, valid.
///
/// # Publish-terminal-state-before-remove invariant
///
/// Both branches uphold: success calls `finish_success` then
/// `in_flight_remove_all`; failure calls `finish_failed` then
/// `in_flight_remove_all`.
pub fn get_or_fill_bulk_async_sync(
    inode: &Arc<VfsInode>,
    start_page: u64,
    page_count: u64,
    bulk_fill_fn: impl FnOnce() -> Result<Vec<u8>, Error>,
) -> Result<(), Error> {
    assert!(
        page_count > 0,
        "get_or_fill_bulk_async_sync: page_count must be > 0"
    );

    'outer: loop {
        // --- Install phase ---
        // Build ONE handle covering the entire range.
        let handle = PageFillHandle::new(Arc::downgrade(inode), start_page, page_count as u32);

        // Try to install at every index atomically under one in_flight lock.
        let park_on: Option<Arc<PageFillHandle>> = {
            let mut inflight = inode
                .pages
                .in_flight
                .lock_ranked(RANK_IN_FLIGHT, "InodePages.in_flight (bulk-install)");

            let mut conflict: Option<Arc<PageFillHandle>> = None;
            for idx in start_page..(start_page + page_count) {
                if let Some(existing) = inflight.get(&idx) {
                    conflict = Some(Arc::clone(existing));
                    break;
                }
            }

            if let Some(c) = conflict {
                // At least one index is busy. Abort the bulk install; park below.
                Some(c)
            } else {
                // All indices free: install the same Arc at every index.
                for idx in start_page..(start_page + page_count) {
                    inflight.insert(idx, Arc::clone(&handle));
                }
                None
            }
        };

        if let Some(conflict) = park_on {
            // Park until the conflicting fill finishes, then retry the whole bulk.
            conflict
                .waiters
                .wait_until(|| conflict.load_state() != FILL_PENDING);
            drop(conflict);
            continue 'outer;
        }

        // --- Handle is installed. Register with owned_ops for cancel hookup. ---
        let op: Arc<dyn CancellableOp> = Arc::clone(&handle) as Arc<dyn CancellableOp>;
        let push_ok = sched()
            .current_thread()
            .map(|t| t.owned_ops_push(op).is_ok())
            .unwrap_or(false);

        if !push_ok {
            // owned_ops full: cannot register for cancellation. If the issuer
            // dies mid-fill, the handle would be stranded in Pending. Safer to
            // abort the bulk entirely. Wake any readers that latched onto our
            // just-installed handle (publish FAILED before removing).
            handle.finish_failed();
            in_flight_remove_all(inode, &handle);
            return Err(Error::IoError);
        }

        // --- Allocate frames (one per page in the range). ---
        let mut frames: Vec<FrameDrop> = Vec::with_capacity(page_count as usize);
        for _ in 0..page_count {
            match frame_allocator().allocate_frame() {
                Some(f) => frames.push(FrameDrop::new(f)),
                None => {
                    // Out of memory: failure path — publish before removing.
                    handle.finish_failed();
                    in_flight_remove_all(inode, &handle);
                    sched().current_thread().map(|t| {
                        t.owned_ops_remove(Arc::as_ptr(&handle) as *const ());
                    });
                    // frames Vec drops here, returning all partially-allocated frames.
                    return Err(Error::IoError);
                }
            }
        }

        // --- Call bulk_fill_fn with no InodePages lock held. ---
        let fill_result = bulk_fill_fn();

        match fill_result {
            Ok(data) => {
                // Wrap each frame into an Arc<CachedPage>, copying the chunk.
                let mut pages_to_insert: Vec<(u64, Arc<CachedPage>)> =
                    Vec::with_capacity(page_count as usize);

                for (i, fd) in frames.into_iter().enumerate() {
                    let data_offset = i * 4096;
                    {
                        let slice = unsafe { fd.frame_slice_mut() };
                        let available = data.len().saturating_sub(data_offset);
                        let copy = available.min(4096);
                        if copy > 0 {
                            slice[..copy].copy_from_slice(&data[data_offset..data_offset + copy]);
                        }
                        if copy < 4096 {
                            slice[copy..].fill(0);
                        }
                    }
                    let frame = fd.forget();
                    let page = Arc::new(CachedPage::new(frame));
                    pages_to_insert.push((start_page + i as u64, page));
                }

                // Publish all N pages under a single pages(40) critical section.
                {
                    let mut pages =
                        ranked_lock!(RANK_PAGES, "InodePages.pages (bulk)", inode.pages.pages);
                    for (idx, page) in pages_to_insert {
                        // Defensive: if another thread already inserted this page, keep
                        // theirs and drop ours (CachedPage::drop returns the frame).
                        pages.entry(idx).or_insert(page);
                    }
                }

                // Publish-terminal-state-before-remove: success branch.
                handle.finish_success();
                in_flight_remove_all(inode, &handle);
                sched().current_thread().map(|t| {
                    t.owned_ops_remove(Arc::as_ptr(&handle) as *const ());
                });
                return Ok(());
            }
            Err(e) => {
                // frames Vec drops here, returning all allocated frames.
                // Publish-terminal-state-before-remove: failure branch.
                handle.finish_failed();
                in_flight_remove_all(inode, &handle);
                sched().current_thread().map(|t| {
                    t.owned_ops_remove(Arc::as_ptr(&handle) as *const ());
                });
                return Err(e);
            }
        }
    }
}
