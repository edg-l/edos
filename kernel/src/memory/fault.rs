use alloc::sync::Arc;
use alloc::vec::Vec;

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
        vma::{VmaBacking, VmaFlags, VmaProt, VmaSet},
    },
    thread::scheduler::sched,
};

use x86_64::structures::idt::PageFaultErrorCode;

/// Info extracted from a VMA needed to resolve a demand fault.
/// Extracted while holding the VMA lock, used after dropping it.
pub struct FaultInfo {
    pub pt_flags: PageTableFlags,
    pub elf_info: Option<(Arc<Vec<u8>>, u64, u64, u64, VirtAddr)>,
}

/// Look up VMA for a fault address and extract fault resolution info.
/// Returns None if no VMA covers the address, the VMA isn't lazy, or
/// the backing type isn't supported for demand faulting.
pub fn lookup_fault_vma(vmas: &VmaSet, fault_addr: VirtAddr) -> Option<FaultInfo> {
    let vma = vmas.find(fault_addr)?;
    if !vma.flags.contains(VmaFlags::LAZY) {
        return None;
    }

    let mut pt_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if vma.prot.contains(VmaProt::WRITE) {
        pt_flags |= PageTableFlags::WRITABLE;
    }
    if !vma.prot.contains(VmaProt::EXEC) {
        pt_flags |= PageTableFlags::NO_EXECUTE;
    }

    let elf_info = match &vma.backing {
        VmaBacking::Anonymous | VmaBacking::Stack => None,
        VmaBacking::ElfSegment {
            elf_data,
            file_offset,
            file_size,
            vaddr_offset,
        } => Some((
            elf_data.clone(),
            *file_offset,
            *file_size,
            *vaddr_offset,
            vma.start,
        )),
        _ => return None,
    };

    Some(FaultInfo { pt_flags, elf_info })
}

/// Fault in a single page given pre-extracted VMA info.
/// Allocates a zero-filled frame, optionally fills from ELF data, and maps it.
/// Can be called after dropping the VMA lock.
/// Returns true if the page was mapped (or was already present from a race).
pub fn fault_in_page(
    fault_addr: VirtAddr,
    info: &FaultInfo,
    cr3: PhysFrame,
    phys_offset: VirtAddr,
) -> bool {
    // Allocate a physical frame
    let frame = match frame_allocator().allocate_frame() {
        Some(f) => f,
        None => return false,
    };

    // Zero the frame via HHDM
    let frame_virt = phys_offset + frame.start_address().as_u64();
    unsafe {
        core::ptr::write_bytes(frame_virt.as_mut_ptr::<u8>(), 0, 4096);
    }

    // For ELF-backed pages, fill from the stored ELF data
    let page_addr = fault_addr.align_down(4096u64);
    if let Some((ref elf_data, file_offset, file_size, vaddr_offset, vma_start)) = info.elf_info {
        let page_off_in_vma = page_addr.as_u64() - vma_start.as_u64();
        let seg_start = page_off_in_vma.saturating_sub(vaddr_offset);
        let seg_end = (page_off_in_vma + 4096).saturating_sub(vaddr_offset);
        let copy_start = seg_start.min(file_size);
        let copy_end = seg_end.min(file_size);

        if copy_end > copy_start {
            let elf_src_offset = (file_offset + copy_start) as usize;
            let elf_src_end = (file_offset + copy_end) as usize;
            if elf_src_end <= elf_data.len() {
                let dst_offset = if page_off_in_vma < vaddr_offset {
                    (vaddr_offset - page_off_in_vma) as usize
                } else {
                    0usize
                };
                let copy_len = (copy_end - copy_start) as usize;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        elf_data[elf_src_offset..].as_ptr(),
                        frame_virt.as_mut_ptr::<u8>().add(dst_offset),
                        copy_len,
                    );
                }
            }
        }
    }

    // Map the page
    let success = unsafe { map_page_direct(cr3, page_addr, frame, info.pt_flags, phys_offset) };

    if success {
        x86_64::instructions::tlb::flush(page_addr);
        true
    } else {
        // Race: check if another CPU/path already mapped it
        if is_page_present(cr3, page_addr, phys_offset) {
            unsafe { frame_allocator().deallocate_frame(frame) };
            true
        } else {
            unsafe { frame_allocator().deallocate_frame(frame) };
            false
        }
    }
}

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

    // Permission checks from the hardware error code
    let is_write = error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE);
    if is_write && !vma.prot.contains(VmaProt::WRITE) {
        return false; // Write to read-only VMA
    }
    let is_exec = error_code.contains(PageFaultErrorCode::INSTRUCTION_FETCH);
    if is_exec && !vma.prot.contains(VmaProt::EXEC) {
        return false; // Execute on no-exec VMA
    }

    let fault_info = match lookup_fault_vma(&vmas, fault_addr) {
        Some(info) => info,
        None => return false,
    };

    // Drop locks before allocating (frame_allocator uses its own lock)
    drop(vmas);
    drop(user_read);

    let (cr3_frame, _) = Cr3::read();
    let phys_offset = boot_info().physical_memory_offset;

    fault_in_page(fault_addr, &fault_info, cr3_frame, phys_offset)
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
