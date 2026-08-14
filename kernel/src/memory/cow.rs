use x86_64::{
    VirtAddr,
    registers::control::Cr3,
    structures::paging::{FrameAllocator, PageTable, PageTableFlags, PhysFrame, Size4KiB},
};

use crate::{
    boot::boot_info,
    debug::lock_order::RANK_VMAS,
    memory::{
        frame_allocator::frame_allocator,
        mapper::COW_BIT,
        vma::{VmaBacking, VmaProt, VmaSet},
    },
    ranked_lock,
    thread::scheduler::current_thread,
};

/// A `&mut PageTable` over the frame's physical-memory-offset mapping.
fn pt_from_frame(frame: PhysFrame) -> &'static mut PageTable {
    let virt = boot_info().physical_memory_offset + frame.start_address().as_u64();
    unsafe { &mut *virt.as_mut_ptr::<PageTable>() }
}

/// Walk the parent's user-space page tables and build a COW copy for the child.
///
/// - Kernel half (entries 256..512) is copied from the kernel PML4 (shared).
/// - For each anonymous leaf PTE: strip WRITABLE, set COW_BIT on both parent
///   and child, and increment the frame refcount.
/// - For SHM (Shared) and Physical/MMIO PTEs: copy the entry as-is.
/// - All intermediate page table frames (PML3/PML2/PML1) are newly allocated
///   for the child; the page-table structure is not shared.
///
/// Returns `None` when the allocator runs out of frames; everything the walk
/// took is given back first, so the parent is left as usable as it was.
///
/// Flushes the local TLB after modifying parent PTEs.
///
/// # Safety
///
/// Must be called while holding the parent's address space (i.e. the parent's
/// CR3 is active or the parent's page tables are otherwise accessible via the
/// physical memory offset). The caller must ensure no other thread is
/// concurrently modifying the parent's page tables.
pub unsafe fn clone_user_page_tables_cow(
    parent_cr3: PhysFrame,
    parent_vmas: &VmaSet,
) -> Option<PhysFrame> {
    // Allocate new PML4 for the child.
    let child_pml4_frame = frame_allocator().allocate_frame()?;

    let child_pml4 = pt_from_frame(child_pml4_frame);
    child_pml4.zero();

    // Copy kernel half from the kernel PML4 (boot-time CR3), not from the
    // parent -- all processes share the same upper-half kernel mappings.
    let kernel_cr3 = boot_info().cr3;
    let kernel_pml4 = pt_from_frame(kernel_cr3.0);
    for i in 256..512 {
        child_pml4[i] = kernel_pml4[i].clone();
    }

    let built = unsafe { clone_user_half(parent_cr3, parent_vmas, child_pml4) };

    // Flush TLB on all CPUs: parent PTEs were changed from writable to
    // read-only (COW). Any CPU with a cached writable entry could write
    // through it without triggering a COW fault. The partial walk of a
    // failed clone changed just as many, so this runs on both paths.
    crate::memory::tlb::tlb_shootdown_all();

    if built.is_none() {
        unsafe { free_child_user_half(child_pml4) };
        unsafe { frame_allocator().deallocate_frame(child_pml4_frame) };
        return None;
    }

    Some(child_pml4_frame)
}

/// Build the child's user half (PML4 entries 0..256) from the parent's.
///
/// `None` means an intermediate table could not be allocated. What has been
/// built by then is a walkable tree, since a child entry is only written after
/// the table it points at is allocated and zeroed, so the caller can hand it
/// to [`free_child_user_half`].
///
/// # Safety
///
/// Same contract as [`clone_user_page_tables_cow`].
unsafe fn clone_user_half(
    parent_cr3: PhysFrame,
    parent_vmas: &VmaSet,
    child_pml4: &mut PageTable,
) -> Option<()> {
    let parent_pml4 = pt_from_frame(parent_cr3);

    // Walk the user half (entries 0..256).
    for pml4_idx in 0..256usize {
        let pml4_entry = &mut parent_pml4[pml4_idx];
        if !pml4_entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }

        // Allocate a child PML3 frame.
        let child_pml3_frame = frame_allocator().allocate_frame()?;
        let child_pml3 = pt_from_frame(child_pml3_frame);
        child_pml3.zero();

        // Point child PML4 entry to the new child PML3.
        child_pml4[pml4_idx].set_addr(child_pml3_frame.start_address(), pml4_entry.flags());

        let parent_pml3 = pt_from_frame(PhysFrame::containing_address(pml4_entry.addr()));

        for pml3_idx in 0..512usize {
            let pml3_entry = &mut parent_pml3[pml3_idx];
            if !pml3_entry.flags().contains(PageTableFlags::PRESENT) {
                continue;
            }

            // Allocate a child PML2 frame.
            let child_pml2_frame = frame_allocator().allocate_frame()?;
            let child_pml2 = pt_from_frame(child_pml2_frame);
            child_pml2.zero();

            child_pml3[pml3_idx].set_addr(child_pml2_frame.start_address(), pml3_entry.flags());

            let parent_pml2 = pt_from_frame(PhysFrame::containing_address(pml3_entry.addr()));

            for pml2_idx in 0..512usize {
                let pml2_entry = &mut parent_pml2[pml2_idx];
                if !pml2_entry.flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }

                // Allocate a child PML1 (page table) frame.
                let child_pml1_frame = frame_allocator().allocate_frame()?;
                let child_pml1 = pt_from_frame(child_pml1_frame);
                child_pml1.zero();

                child_pml2[pml2_idx].set_addr(child_pml1_frame.start_address(), pml2_entry.flags());

                let parent_pml1 = pt_from_frame(PhysFrame::containing_address(pml2_entry.addr()));

                for pml1_idx in 0..512usize {
                    let pml1_entry = &mut parent_pml1[pml1_idx];
                    if !pml1_entry.flags().contains(PageTableFlags::PRESENT) {
                        continue;
                    }

                    let data_frame: PhysFrame<Size4KiB> =
                        PhysFrame::containing_address(pml1_entry.addr());

                    // Reconstruct the virtual address this PTE maps.
                    let virt = VirtAddr::new(
                        ((pml4_idx as u64) << 39)
                            | ((pml3_idx as u64) << 30)
                            | ((pml2_idx as u64) << 21)
                            | ((pml1_idx as u64) << 12),
                    );

                    if is_special_mapping(virt, parent_vmas) {
                        // SHM or Physical/MMIO: copy PTE unchanged, no COW.
                        // Bump refcount so deallocate_frame on child exit
                        // decrements instead of freeing the frame while
                        // the parent (or SHM object) still references it.
                        child_pml1[pml1_idx] = pml1_entry.clone();
                        frame_allocator().inc_refcount(data_frame);
                    } else {
                        // Anonymous page: make both parent and child read-only
                        // with COW_BIT set, and bump the frame refcount.
                        let flags = pml1_entry.flags();
                        let cow_flags = (flags - PageTableFlags::WRITABLE) | COW_BIT;
                        pml1_entry.set_addr(data_frame.start_address(), cow_flags);
                        child_pml1[pml1_idx].set_addr(data_frame.start_address(), cow_flags);
                        frame_allocator().inc_refcount(data_frame);
                    }
                }
            }
        }
    }

    Some(())
}

/// Give back everything [`clone_user_half`] took, leaf frames included.
///
/// The parent's own PTEs keep the COW flags the walk gave them: with the
/// child's reference dropped the frame is solely owned again, so the parent's
/// first write to one faults once and [`handle_cow_fault`] makes it writable
/// in place.
///
/// # Safety
///
/// `child_pml4` must be a child table built by [`clone_user_half`] and
/// unreachable from any CR3.
unsafe fn free_child_user_half(child_pml4: &mut PageTable) {
    let mut alloc = frame_allocator();

    for pml4_idx in 0..256usize {
        if !child_pml4[pml4_idx]
            .flags()
            .contains(PageTableFlags::PRESENT)
        {
            continue;
        }
        let pml3_frame = PhysFrame::containing_address(child_pml4[pml4_idx].addr());
        let pml3 = pt_from_frame(pml3_frame);

        for pml3_idx in 0..512usize {
            if !pml3[pml3_idx].flags().contains(PageTableFlags::PRESENT) {
                continue;
            }
            let pml2_frame = PhysFrame::containing_address(pml3[pml3_idx].addr());
            let pml2 = pt_from_frame(pml2_frame);

            for pml2_idx in 0..512usize {
                if !pml2[pml2_idx].flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }
                let pml1_frame = PhysFrame::containing_address(pml2[pml2_idx].addr());
                let pml1 = pt_from_frame(pml1_frame);

                for pml1_idx in 0..512usize {
                    if !pml1[pml1_idx].flags().contains(PageTableFlags::PRESENT) {
                        continue;
                    }
                    // The exact inverse of the walk's inc_refcount: the parent
                    // still holds this frame, so it must not be freed here.
                    alloc.dec_refcount(PhysFrame::containing_address(pml1[pml1_idx].addr()));
                }
                unsafe { alloc.deallocate_frame(pml1_frame) };
            }
            unsafe { alloc.deallocate_frame(pml2_frame) };
        }
        unsafe { alloc.deallocate_frame(pml3_frame) };
    }
}

/// Returns true if `virt` falls within a SHM, Physical, or MAP_SHARED FileBacked mapping.
/// These PTEs are copied verbatim into the child (no COW stripping) during fork.
fn is_special_mapping(virt: VirtAddr, vmas: &VmaSet) -> bool {
    if let Some(vma) = vmas.find(virt) {
        return matches!(
            vma.backing,
            VmaBacking::SharedMemory { .. }
                | VmaBacking::Physical { .. }
                | VmaBacking::FileBacked { shared: true, .. }
        );
    }
    false
}

/// Whether the VMA covering `fault_addr` permits writes.
///
/// A fault with no thread or no VMA behind it is not this path's to resolve, so
/// both answer `false` and let the demand path name the rejection.
fn fault_addr_is_writable(fault_addr: VirtAddr) -> bool {
    let Some(thread) = current_thread() else {
        return false;
    };
    let Some(user) = &thread.user else {
        return false;
    };
    let user_read = user.read();
    let vmas = ranked_lock!(RANK_VMAS, "user.vmas", user_read.vmas);
    vmas.find(fault_addr)
        .is_some_and(|vma| vma.prot.contains(VmaProt::WRITE))
}

/// Handle a COW page fault for `fault_addr`.
///
/// Returns `true` if the fault was a COW fault and has been resolved.
/// Returns `false` if the fault was not COW (caller should handle it).
///
/// # Safety
///
/// Must be called from the page fault handler with the current process's
/// address space active in CR3. Interrupts may be enabled.
pub unsafe fn handle_cow_fault(fault_addr: VirtAddr) -> bool {
    // The VMA is the authority on what a mapping may do, and this path is the
    // one that hands out write permission. Without the check a page that is
    // read-only to its VMA still carries COW_BIT from the fork walk, so the
    // copy would succeed and the write would land on a mapping that never
    // allowed one. Declining leaves the fault to the demand path, which
    // rejects it as WriteToReadOnly.
    if !fault_addr_is_writable(fault_addr) {
        return false;
    }

    let phys_offset = boot_info().physical_memory_offset;

    let (cr3_frame, _) = Cr3::read();
    let pml4 = pt_from_frame(cr3_frame);

    let addr = fault_addr.as_u64();
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pml3_idx = ((addr >> 30) & 0x1FF) as usize;
    let pml2_idx = ((addr >> 21) & 0x1FF) as usize;
    let pml1_idx = ((addr >> 12) & 0x1FF) as usize;

    let pml4_entry = &pml4[pml4_idx];
    if !pml4_entry.flags().contains(PageTableFlags::PRESENT) {
        return false;
    }
    let pml3 = pt_from_frame(PhysFrame::containing_address(pml4_entry.addr()));

    let pml3_entry = &pml3[pml3_idx];
    if !pml3_entry.flags().contains(PageTableFlags::PRESENT) {
        return false;
    }
    let pml2 = pt_from_frame(PhysFrame::containing_address(pml3_entry.addr()));

    let pml2_entry = &pml2[pml2_idx];
    if !pml2_entry.flags().contains(PageTableFlags::PRESENT) {
        return false;
    }
    let pml1 = pt_from_frame(PhysFrame::containing_address(pml2_entry.addr()));

    let pte = &mut pml1[pml1_idx];
    if !pte.flags().contains(COW_BIT) {
        return false;
    }

    let old_frame: PhysFrame<Size4KiB> = PhysFrame::containing_address(pte.addr());
    let mut alloc = frame_allocator();

    // Re-check COW_BIT under lock: another CPU (in a multi-threaded process)
    // may have already resolved this fault.
    if !pte.flags().contains(COW_BIT) {
        return true; // already resolved
    }

    let rc = alloc.refcount(old_frame);

    if rc <= 1 {
        // Sole owner: just make the page writable.
        let new_flags = (pte.flags() - COW_BIT) | PageTableFlags::WRITABLE;
        pte.set_addr(old_frame.start_address(), new_flags);
        drop(alloc);
        x86_64::instructions::tlb::flush(fault_addr);
    } else {
        // Shared: copy the frame, point PTE to new frame.
        let Some(new_frame) = alloc.allocate_frame() else {
            // Out of frames. The fault is real and unresolvable, so report it
            // unhandled: the fault handler kills the faulting process rather
            // than the machine.
            crate::emergency_println!(
                "COW fault at {:p}: out of frames, cannot copy shared page",
                fault_addr
            );
            return false;
        };

        let src_ptr = (phys_offset + old_frame.start_address().as_u64()).as_ptr::<u8>();
        let dst_ptr = (phys_offset + new_frame.start_address().as_u64()).as_mut_ptr::<u8>();
        unsafe { core::ptr::copy_nonoverlapping(src_ptr, dst_ptr, 4096) };

        alloc.dec_refcount(old_frame);

        let new_flags = (pte.flags() - COW_BIT) | PageTableFlags::WRITABLE;
        pte.set_addr(new_frame.start_address(), new_flags);
        drop(alloc);
        x86_64::instructions::tlb::flush(fault_addr);
    }

    true
}
