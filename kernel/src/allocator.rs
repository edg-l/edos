//! Kernel allocator

use linked_list_allocator::LockedHeap;
use x86_64::structures::paging::PageTableFlags;

use crate::{
    memory::{KERNEL_HEAP, KERNEL_HEAP_SIZE, mapper::memory_mapper},
    println,
};

#[global_allocator]
pub static ALLOCATOR: LockedHeap = LockedHeap::empty();

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
            .init(heap_start.as_mut_ptr(), heap_size as usize);
    }
}

pub fn print_alloc_stats() {
    let used = ALLOCATOR.lock().used();
    let size = ALLOCATOR.lock().size();

    println!("Kernel heap {} kb / {} kb", used / 1024, size / 1024);
}
