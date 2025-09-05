use core::alloc::{GlobalAlloc, Layout};

use linked_list_allocator::LockedHeap;
use spin::Once;

use crate::{MAP_ANONYMOUS, PROT_READ, PROT_WRITE, sys_mmap};

#[global_allocator]
pub static ALLOCATOR: Locked = Locked::new();

unsafe impl GlobalAlloc for Locked {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { self.lock().alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.lock().dealloc(ptr, layout) };
    }
}

/// Wrapper for a spin Mutex, useful to implement traits.
pub struct Locked {
    inner: Once<LockedHeap>,
}

impl Default for Locked {
    fn default() -> Self {
        Self::new()
    }
}

impl Locked {
    pub const fn new() -> Self {
        Locked { inner: Once::new() }
    }

    pub fn lock(&self) -> &LockedHeap {
        self.inner.call_once(|| {
            let ptr = sys_mmap(0, 1024 * 1024 * 64, PROT_WRITE | PROT_READ, MAP_ANONYMOUS);

            unsafe { LockedHeap::new(ptr, 1024 * 1024 * 64) }
        })
    }
}
