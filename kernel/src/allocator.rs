//! Kernel allocator

use buddy_system_allocator::LockedHeap;
use x86_64::structures::paging::PageTableFlags;

use crate::{
    memory::{KERNEL_HEAP, KERNEL_HEAP_SIZE, mapper::memory_mapper},
    println,
};

#[global_allocator]
pub static ALLOCATOR: LockedHeap<32> = LockedHeap::empty();

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
            .lock()
            .init(heap_start.as_u64() as usize, heap_size as usize);
    }
}

pub fn print_alloc_stats() {
    let used = ALLOCATOR.lock().stats_alloc_actual();
    let size = ALLOCATOR.lock().stats_total_bytes();

    println!("Kernel heap {} kb / {} kb", used / 1024, size / 1024);
}
