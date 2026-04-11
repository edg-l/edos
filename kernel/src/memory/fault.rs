use x86_64::{
    VirtAddr,
    registers::control::Cr3,
    structures::paging::{
        FrameAllocator, PageTable, PageTableFlags, PhysFrame, Size4KiB, page_table::PageTableEntry,
    },
};

use crate::{
    boot::boot_info,
    memory::{
        frame_allocator::frame_allocator,
        vma::{VmaBacking, VmaFlags, VmaProt},
    },
    thread::scheduler::sched,
};

use x86_64::structures::idt::PageFaultErrorCode;

/// Handle a demand page fault for userspace.
///
/// Called from the page fault handler when a userspace access faults on a
/// non-present page. Looks up the faulting address in the current thread's
/// VmaSet and, if valid, allocates a zero-filled frame and maps it.
///
/// Returns `true` if the fault was resolved, `false` if invalid (caller kills thread).
///
/// # Safety
/// Must be called from the page fault handler with the faulting thread's CR3 active.
pub unsafe fn handle_demand_fault(fault_addr: VirtAddr, error_code: PageFaultErrorCode) -> bool {
    // Don't handle protection violations here (those go to COW handler)
    if error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION) {
        return false;
    }

    // Get current thread's VmaSet
    let thread = match sched().current_thread() {
        Some(t) => t,
        None => return false,
    };
    let user = match &thread.user {
        Some(u) => u,
        None => return false,
    };

    // Read the VmaSet under spin lock (IST-safe)
    let user_read = user.read();
    let vmas = user_read.vmas.lock();

    let vma = match vmas.find(fault_addr) {
        Some(v) => v,
        None => return false, // No VMA covers this address
    };

    // Check if this VMA is lazy (demand-paged)
    if !vma.flags.contains(VmaFlags::LAZY) {
        return false; // Eagerly mapped VMA shouldn't have non-present pages
    }

    // Permission checks
    let is_write = error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE);
    if is_write && !vma.prot.contains(VmaProt::WRITE) {
        return false; // Write to read-only VMA
    }
    let is_exec = error_code.contains(PageFaultErrorCode::INSTRUCTION_FETCH);
    if is_exec && !vma.prot.contains(VmaProt::EXEC) {
        return false; // Execute on no-exec VMA
    }

    // Only handle Anonymous and Stack backing for now
    match &vma.backing {
        VmaBacking::Anonymous | VmaBacking::Stack => {}
        _ => return false,
    }

    // Build page table flags from VMA protection
    let mut pt_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if vma.prot.contains(VmaProt::WRITE) {
        pt_flags |= PageTableFlags::WRITABLE;
    }
    if !vma.prot.contains(VmaProt::EXEC) {
        pt_flags |= PageTableFlags::NO_EXECUTE;
    }

    // Drop the VMA lock before allocating (frame_allocator uses its own lock)
    drop(vmas);
    drop(user_read);

    // Allocate a physical frame
    let frame = match frame_allocator().allocate_frame() {
        Some(f) => f,
        None => return false, // OOM
    };

    // Zero the frame via HHDM
    let phys_offset = boot_info().physical_memory_offset;
    let frame_virt = phys_offset + frame.start_address().as_u64();
    unsafe {
        core::ptr::write_bytes(frame_virt.as_mut_ptr::<u8>(), 0, 4096);
    }

    // Map the page into the faulting process's page table via HHDM.
    // We walk the page table directly (same approach as handle_cow_fault)
    // to avoid needing a MemoryManager lock.
    let (cr3_frame, _) = Cr3::read();
    let page_addr = fault_addr.align_down(4096u64);

    let success = unsafe { map_page_direct(cr3_frame, page_addr, frame, pt_flags, phys_offset) };

    if success {
        x86_64::instructions::tlb::flush(page_addr);
        true
    } else {
        // Map failed: check if the page is already present (race with another CPU)
        if is_page_present(cr3_frame, page_addr, phys_offset) {
            // Another CPU already mapped this page - free our frame and succeed
            unsafe { frame_allocator().deallocate_frame(frame) };
            true
        } else {
            // Real failure - free frame and report
            unsafe { frame_allocator().deallocate_frame(frame) };
            false
        }
    }
}

/// Map a single page directly into a page table via HHDM.
/// Allocates intermediate page table frames as needed.
/// Returns false if the leaf PTE is already present (race condition) or on
/// allocation failure.
unsafe fn map_page_direct(
    cr3: PhysFrame,
    vaddr: VirtAddr,
    frame: PhysFrame<Size4KiB>,
    flags: PageTableFlags,
    phys_offset: VirtAddr,
) -> bool {
    let pt_from_frame = |f: PhysFrame| -> &'static mut PageTable {
        let virt = phys_offset + f.start_address().as_u64();
        unsafe { &mut *virt.as_mut_ptr::<PageTable>() }
    };

    let addr = vaddr.as_u64();
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pml3_idx = ((addr >> 30) & 0x1FF) as usize;
    let pml2_idx = ((addr >> 21) & 0x1FF) as usize;
    let pml1_idx = ((addr >> 12) & 0x1FF) as usize;

    let intermediate_flags =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;

    let pml4 = pt_from_frame(cr3);

    // PML4 -> PML3
    let pml3_frame =
        match unsafe { ensure_table_entry(&mut pml4[pml4_idx], intermediate_flags, phys_offset) } {
            Some(f) => f,
            None => return false,
        };
    let pml3 = pt_from_frame(pml3_frame);

    // PML3 -> PML2
    let pml2_frame =
        match unsafe { ensure_table_entry(&mut pml3[pml3_idx], intermediate_flags, phys_offset) } {
            Some(f) => f,
            None => return false,
        };
    let pml2 = pt_from_frame(pml2_frame);

    // PML2 -> PML1
    let pml1_frame =
        match unsafe { ensure_table_entry(&mut pml2[pml2_idx], intermediate_flags, phys_offset) } {
            Some(f) => f,
            None => return false,
        };
    let pml1 = pt_from_frame(pml1_frame);

    // Check the leaf PTE
    let pte = &mut pml1[pml1_idx];
    if pte.flags().contains(PageTableFlags::PRESENT) {
        return false; // Already mapped (race with another CPU)
    }

    // Map the page
    pte.set_addr(frame.start_address(), flags);
    true
}

/// Ensure a page table entry points to a valid next-level table.
/// If not present, allocate a new frame and initialize it.
/// Returns the frame of the next-level table, or None on allocation failure.
unsafe fn ensure_table_entry(
    entry: &mut PageTableEntry,
    flags: PageTableFlags,
    phys_offset: VirtAddr,
) -> Option<PhysFrame> {
    if entry.flags().contains(PageTableFlags::PRESENT) {
        Some(PhysFrame::containing_address(entry.addr()))
    } else {
        let new_frame = frame_allocator().allocate_frame()?;
        // Zero the new table
        let virt = phys_offset + new_frame.start_address().as_u64();
        unsafe {
            core::ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, 4096);
        }
        entry.set_addr(new_frame.start_address(), flags);
        Some(new_frame)
    }
}

/// Check if a page is present in the page table.
fn is_page_present(cr3: PhysFrame, vaddr: VirtAddr, phys_offset: VirtAddr) -> bool {
    let pt_from_frame = |f: PhysFrame| -> &'static PageTable {
        let virt = phys_offset + f.start_address().as_u64();
        unsafe { &*virt.as_ptr::<PageTable>() }
    };

    let addr = vaddr.as_u64();
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pml3_idx = ((addr >> 30) & 0x1FF) as usize;
    let pml2_idx = ((addr >> 21) & 0x1FF) as usize;
    let pml1_idx = ((addr >> 12) & 0x1FF) as usize;

    let pml4 = pt_from_frame(cr3);
    if !pml4[pml4_idx].flags().contains(PageTableFlags::PRESENT) {
        return false;
    }
    let pml3 = pt_from_frame(PhysFrame::containing_address(pml4[pml4_idx].addr()));
    if !pml3[pml3_idx].flags().contains(PageTableFlags::PRESENT) {
        return false;
    }
    let pml2 = pt_from_frame(PhysFrame::containing_address(pml3[pml3_idx].addr()));
    if !pml2[pml2_idx].flags().contains(PageTableFlags::PRESENT) {
        return false;
    }
    let pml1 = pt_from_frame(PhysFrame::containing_address(pml2[pml2_idx].addr()));
    pml1[pml1_idx].flags().contains(PageTableFlags::PRESENT)
}
