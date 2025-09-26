//! Kernel allocator

use core::{
    alloc::GlobalAlloc,
    ptr::{NonNull, null_mut},
};

use buddy_system_allocator::Heap;
use x86_64::structures::paging::PageTableFlags;

use crate::{
    memory::{KERNEL_HEAP, KERNEL_HEAP_SIZE, mapper::memory_mapper},
    println,
    thread::irqlock::IrqSpinlock,
};

#[global_allocator]
pub static ALLOCATOR: Allocator = Allocator {
    inner: IrqSpinlock::new(Heap::empty()),
};

pub struct Allocator {
    inner: IrqSpinlock<Heap<32>>,
}

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        self.inner
            .lock()
            .alloc(layout)
            .map(|x| x.as_ptr())
            .unwrap_or(null_mut())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        self.inner
            .lock()
            .dealloc(unsafe { NonNull::new_unchecked(ptr) }, layout);
    }
}

pub fn init_heap() {
    let heap_start = KERNEL_HEAP;
    let heap_end = KERNEL_HEAP + KERNEL_HEAP_SIZE;
    let heap_size = heap_end - heap_start;

    let mut mapper = memory_mapper();
    mapper
        .map_memory(
            heap_start,
            KERNEL_HEAP_SIZE,
            PageTableFlags::WRITABLE | PageTableFlags::GLOBAL,
        )
        .expect("failed to map heap");

    println!("Mapped kernel heap at {:p}-{:p}", heap_end, heap_end);

    unsafe {
        ALLOCATOR
            .inner
            .lock()
            .init(heap_start.as_u64() as usize, heap_size as usize);
    }
}

pub fn print_alloc_stats() {
    let used = ALLOCATOR.inner.lock().stats_alloc_actual();
    let size = ALLOCATOR.inner.lock().stats_total_bytes();

    println!("Kernel heap {} kb / {} kb", used / 1024, size / 1024);
}
