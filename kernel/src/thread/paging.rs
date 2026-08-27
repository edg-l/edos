use x86_64::structures::paging::{FrameAllocator, PageTable, PhysFrame};

use crate::memory::{frame_allocator::frame_allocator, get_virt_addr_from_phys_offset};

/// Allocate a fresh PML4 for a new address space: zeroed, with the kernel's
/// higher half (entries 256..512) copied in from `kernel_pml4`.
///
/// # Safety
/// The physical-memory offset mapping must be live, since the new frame is
/// written through it. The returned frame is owned by the caller and is not
/// reference-counted: it must be freed exactly once, after nothing can still
/// have it in `CR3`. Its higher half aliases the kernel's own page tables, so
/// freeing it must not descend into those entries.
pub unsafe fn allocate_process_pml4(kernel_pml4: &PageTable) -> PhysFrame {
    let mut alloc = frame_allocator();
    // Allocate new PML4 frame
    let pml4_frame = alloc
        .allocate_frame()
        .expect("Failed to allocate PML4 frame");

    let pml4_virt = get_virt_addr_from_phys_offset(pml4_frame.start_address());
    // SAFETY: `pml4_frame` has just been handed out by the frame allocator, so
    // nothing else owns it, and the HHDM the caller guarantees is live maps it
    // at `pml4_virt`. A frame is page-sized and page-aligned, which is
    // `PageTable`'s layout.
    let pml4: &mut PageTable = unsafe { &mut *pml4_virt.as_mut_ptr() };

    // Zero the entire table
    pml4.zero();

    // Copy higher kernel half mappings
    for i in 256..512 {
        pml4[i] = kernel_pml4[i].clone();
    }

    pml4_frame
}
