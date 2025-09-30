//! Kernel allocator

use core::{alloc::GlobalAlloc, ptr::NonNull};

use buddy_system_allocator::Heap;
use x86_64::{VirtAddr, align_up, structures::paging::PageTableFlags};

use crate::{
    memory::{KERNEL_HEAP, KERNEL_HEAP_SIZE, mapper::memory_mapper, valloc::vmalloc},
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

const MIN_EXPANSION: u64 = 1 << 20; // 1mb

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        {
            let mut heap = self.inner.lock();
            if let Ok(block) = heap.alloc(layout) {
                return block.as_ptr();
            }
        }

        // expansion size
        let padded = layout.pad_to_align();
        let mut need = padded.size() as u64;
        need = align_up(need, 4096);

        // at least 1 MiB, rounded power of two, cap at 1<<32
        let mut chunk = need.next_power_of_two().max(MIN_EXPANSION);
        let max_block: u64 = 1u64 << 32;
        if chunk > max_block {
            chunk = max_block;
        }

        // over-reserve to allow alignment
        let reserve = chunk * 2;
        let raw = vmalloc(reserve); // returns 4 KiB aligned

        // align base up to chunk
        let base = align_up(raw.as_u64(), chunk);
        let end = base + chunk;

        // only map the aligned window
        {
            let mut mapper = memory_mapper();
            mapper
                .map_memory(
                    VirtAddr::new(base),
                    chunk,
                    PageTableFlags::WRITABLE | PageTableFlags::GLOBAL,
                )
                .expect("failed to map heap expansion");
        }

        // add to heap and retry
        {
            let mut heap = self.inner.lock();
            unsafe { heap.add_to_heap(base as usize, end as usize) };
            heap.alloc(layout)
                .map(|b| b.as_ptr())
                .unwrap_or(core::ptr::null_mut())
        }
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
