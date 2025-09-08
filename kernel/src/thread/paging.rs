use x86_64::structures::paging::{FrameAllocator, PageTable, PhysFrame};

use crate::memory::{frame_allocator::frame_allocator, get_virt_addr};

pub unsafe fn allocate_process_pml4(kernel_pml4: &PageTable) -> PhysFrame {
    let mut alloc = frame_allocator();
    // Allocate new PML4 frame
    let pml4_frame = alloc
        .allocate_frame()
        .expect("Failed to allocate PML4 frame");

    let pml4_virt = get_virt_addr(pml4_frame.start_address());
    let pml4: &mut PageTable = unsafe { &mut *pml4_virt.as_mut_ptr() };

    // Zero the entire table
    pml4.zero();

    // Copy higher kernel half mappings
    for i in 256..512 {
        pml4[i] = kernel_pml4[i].clone();
    }

    pml4_frame
}
