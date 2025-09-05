use core::alloc::{GlobalAlloc, Layout};

use linked_list_allocator::LockedHeap;
use spin::Once;

use crate::{Errno, MAP_ANONYMOUS, MAP_PRIVATE, PROT_WRITE, println, sys_errno, sys_mmap};

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
            let ptr = sys_mmap(0, 1024 * 1024 * 64, PROT_WRITE, MAP_ANONYMOUS | MAP_PRIVATE);

            if ptr as u64 == !0u64 {
                let errno = sys_errno() as Errno;

                println!("Error getting heap ptr: {errno:?}");
                panic!();
            }

            println!("Initializing heap: {ptr:p}");

            unsafe { LockedHeap::new(ptr, 1024 * 1024 * 64) }
        })
    }
}
