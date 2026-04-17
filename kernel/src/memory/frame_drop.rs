// Phase 1: used by page_fill.rs; call sites in page_fill.rs are wired in Phase 2.
#![allow(dead_code)]
//! RAII guard for a single physical frame.
//!
//! `FrameDrop` wraps a `PhysFrame` that has been allocated but not yet handed
//! to an `Arc<CachedPage>`. On drop it calls `frame_allocator().deallocate_frame()`
//! so the frame is always returned even on early exit from the publisher path in
//! `get_or_fill_async_sync`.
//!
//! # Lock-order note
//!
//! `FrameDrop::drop` calls `frame_allocator()` which takes the frame-allocator
//! lock at rank 910. Any early-return on the publisher path where a `FrameDrop`
//! is live may release rank 910 inside the caller's lock stack. The caller stacks
//! that reach here are at most `pages(40) -> in_flight(42) -> frame_alloc(910)`,
//! which is ascending and therefore valid. An early-return BEFORE `pages(40)` /
//! `in_flight(42)` are re-taken (e.g. after `fill_fn` fails) runs `FrameDrop::drop`
//! with no InodePages lock held; still ascending because 910 is a leaf.
//!
//! # Usage
//!
//! ```
//! let fd = FrameDrop::new(frame_allocator().allocate_frame().ok_or(Error::IoError)?);
//! fill_fn(fd.frame_slice_mut())?;          // FrameDrop freed on any early return
//! let frame = fd.forget();                  // hand ownership to Arc<CachedPage>
//! let _page = Arc::new(CachedPage::new(frame));
//! ```

use x86_64::structures::paging::PhysFrame;

use crate::memory::{frame_allocator::frame_allocator, get_virt_addr_from_phys_offset};

const PAGE_SIZE: usize = 4096;

/// RAII wrapper around a single 4 KiB `PhysFrame`.
///
/// Calls `frame_allocator().deallocate_frame(frame)` on drop if the frame has
/// not been consumed via `forget()`. Use `forget()` to transfer ownership to an
/// `Arc<CachedPage>`.
pub struct FrameDrop(Option<PhysFrame>);

impl FrameDrop {
    /// Wrap a freshly-allocated frame.
    pub fn new(f: PhysFrame) -> Self {
        Self(Some(f))
    }

    /// Return the wrapped frame (panics if already forgotten).
    pub fn frame(&self) -> PhysFrame {
        self.0
            .expect("FrameDrop: frame already consumed via forget()")
    }

    /// Get a mutable byte slice over the frame via the HHDM mapping.
    ///
    /// # Safety
    ///
    /// Caller must ensure no other live reference to this frame exists.
    pub unsafe fn frame_slice_mut(&self) -> &mut [u8] {
        let frame = self.frame();
        let ptr = get_virt_addr_from_phys_offset(frame.start_address()).as_mut_ptr();
        // SAFETY: frame is a valid 4 KiB physical page mapped at HHDM offset;
        // caller guarantees exclusive access.
        unsafe { core::slice::from_raw_parts_mut(ptr, PAGE_SIZE) }
    }

    /// Consume the guard and return the inner `PhysFrame` WITHOUT deallocating it.
    /// Use this to hand ownership to `Arc::new(CachedPage::new(frame))`.
    pub fn forget(mut self) -> PhysFrame {
        self.0
            .take()
            .expect("FrameDrop: frame already consumed via forget()")
    }
}

impl Drop for FrameDrop {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            // SAFETY: we own this frame and are returning it to the allocator.
            unsafe { frame_allocator().deallocate_frame(f) };
        }
    }
}
