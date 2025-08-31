use x86_64::structures::paging::{FrameAllocator, PageTable, PageTableFlags, PhysFrame};

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

    // Copy kernel mappings (higher half: indices 256-511)
    for i in 256..512 {
        pml4[i] = kernel_pml4[i].clone();
    }

    pml4_frame
}

pub unsafe fn deallocate_lower_half(pml4: &mut PageTable) {
    // Walk indices 0-255 (lower half)
    for i in 0..256 {
        let pml4_entry = &mut pml4[i];

        if !pml4_entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }

        // Get PDPT and recurse
        let pdpt_frame = pml4_entry.frame().unwrap();
        let pdpt = unsafe {
            &mut *get_virt_addr(pdpt_frame.start_address()).as_mut_ptr()
        };

        unsafe { deallocate_pdpt(pdpt) };

        // Free the PDPT frame and clear entry
        unsafe { frame_allocator().deallocate_frame(pdpt_frame) };
        pml4_entry.set_unused();
    }
}

unsafe fn deallocate_pdpt(pdpt: &mut PageTable) {
    for entry in pdpt.iter_mut() {
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }

        if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            // 1GB page, just clear entry
            entry.set_unused();
            continue;
        }

        let pd_frame = entry.frame().unwrap();

        unsafe {
            let pd = &mut *get_virt_addr(pd_frame.start_address()).as_mut_ptr();
            deallocate_pd(pd);
            frame_allocator().deallocate_frame(pd_frame);
        }
        entry.set_unused();
    }
}

unsafe fn deallocate_pd(pd: &mut PageTable) {
    for entry in pd.iter_mut() {
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }

        if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            // 2MB page, just clear entry
            entry.set_unused();
            continue;
        }

        let pt_frame = entry.frame().unwrap();
        unsafe { frame_allocator().deallocate_frame(pt_frame) };
        entry.set_unused();
    }
}
