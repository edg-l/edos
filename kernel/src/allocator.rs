//! Kernel allocator

use linked_list_allocator::LockedHeap;
use x86_64::structures::paging::PageTableFlags;

use crate::{memory::{mapper::memory_mapper, KERNEL_HEAP, KERNEL_HEAP_SIZE}, serial_println};

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

pub fn init_heap() {
    let heap_start = KERNEL_HEAP;
    let heap_end = KERNEL_HEAP + KERNEL_HEAP_SIZE;
    let heap_size = heap_end - heap_start;

    let mut mapper = memory_mapper();
    mapper
        .map_memory(heap_start, KERNEL_HEAP_SIZE, PageTableFlags::WRITABLE)
        .expect("failed to map heap");

    serial_println!("Mapped kernel heap at {:p}-{:p}", heap_end, heap_end);

    unsafe {
        ALLOCATOR
            .lock()
            .init(heap_start.as_mut_ptr(), heap_size as usize);
    }
}
