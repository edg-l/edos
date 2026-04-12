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
    fs::{
        inode::VfsInode,
        page_cache::CachedPage,
        vfs::{fs_by_mount_id, get_or_fill_page, register_dirty_inode},
    },
    memory::{
        frame_allocator::frame_allocator,
        mapper::COW_BIT,
        vma::{VmaBacking, VmaFlags, VmaProt, VmaSet},
    },
    thread::scheduler::sched,
};

use x86_64::structures::idt::PageFaultErrorCode;

/// Source of data for resolving a demand fault.
pub enum FaultSource {
    /// Zero-fill (Anonymous, Stack).
    Zero,
    /// ELF segment data from an in-kernel buffer.
    ElfData {
        elf_data: Arc<Vec<u8>>,
        file_offset: u64,
        file_size: u64,
        vaddr_offset: u64,
        vma_start: VirtAddr,
    },
    /// File-backed (MAP_PRIVATE or MAP_SHARED).
    FileBacked {
        inode: Arc<VfsInode>,
        /// File page index to fetch.
        page_idx: u64,
        /// Arc slot index within the VMA's `pages` Vec, for storing the Arc.
        vma_page_slot: usize,
        /// true for MAP_SHARED mappings; used to mark the page dirty on fault.
        shared: bool,
    },
}

/// Info extracted from a VMA needed to resolve a demand fault.
/// Extracted while holding the VMA lock, used after dropping it.
pub struct FaultInfo {
    pub pt_flags: PageTableFlags,
    pub source: FaultSource,
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

    let source = match &vma.backing {
        VmaBacking::Anonymous | VmaBacking::Stack => FaultSource::Zero,
        VmaBacking::ElfSegment {
            elf_data,
            file_offset,
            file_size,
            vaddr_offset,
        } => FaultSource::ElfData {
            elf_data: elf_data.clone(),
            file_offset: *file_offset,
            file_size: *file_size,
            vaddr_offset: *vaddr_offset,
            vma_start: vma.start,
        },
        VmaBacking::FileBacked {
            inode,
            file_offset,
            shared,
            ..
        } => {
            let page_addr = fault_addr.align_down(4096u64);
            let vma_byte_offset = page_addr.as_u64() - vma.start.as_u64();
            let page_idx = (file_offset + vma_byte_offset) / 4096;
            let vma_page_slot = (vma_byte_offset / 4096) as usize;

            // MAP_PRIVATE: read-only PTE + COW_BIT so that first write copies.
            // MAP_SHARED: respects vma.prot (WRITABLE already set above if PROT_WRITE).
            if !shared {
                pt_flags &= !PageTableFlags::WRITABLE;
                pt_flags |= COW_BIT;
            }

            FaultSource::FileBacked {
                inode: Arc::clone(inode),
                page_idx,
                vma_page_slot,
                shared: *shared,
            }
        }
        _ => return None,
    };

    Some(FaultInfo { pt_flags, source })
}

/// Result of a `fault_in_page` call.
pub struct FaultOutcome {
    /// True if the page is now mapped (successfully faulted in or was already present).
    pub mapped: bool,
    /// For FileBacked faults: the `Arc<CachedPage>` and VMA page-slot index that must be
    /// stored back into the VMA's `pages` vec.  The caller must re-acquire the VmaSet lock
    /// and call `vma.store_cached_page(slot, arc)` (or equivalent) to keep the Arc alive
    /// for the duration of the mapping.
    pub cached_page: Option<(usize, Arc<CachedPage>)>,
}

/// Fault in a single page given pre-extracted VMA info.
/// For Zero/ElfData: allocates a frame, optionally copies ELF data, and maps it.
/// For FileBacked: fetches the page from the inode page cache and maps it read-only + COW_BIT
/// (MAP_PRIVATE) or with full prot flags (MAP_SHARED).
/// Can be called after dropping the VMA lock.
pub fn fault_in_page(
    fault_addr: VirtAddr,
    info: &FaultInfo,
    cr3: PhysFrame,
    phys_offset: VirtAddr,
) -> FaultOutcome {
    let page_addr = fault_addr.align_down(4096u64);

    // --- FileBacked path ---
    if let FaultSource::FileBacked {
        inode,
        page_idx,
        vma_page_slot,
        shared,
    } = &info.source
    {
        // Past-EOF check: if the faulting page starts at or beyond the file size,
        // return false (SIGBUS-equivalent — caller kills the thread).
        if let Some(fs) = fs_by_mount_id(inode.mount_id) {
            if let Ok(file_size) = fs.file_size_ino(inode.ino) {
                if page_idx * 4096 >= file_size {
                    return FaultOutcome {
                        mapped: false,
                        cached_page: None,
                    };
                }
            }
        }

        // Get or fill the page from the inode page cache.
        // `get_or_fill_page` calls `page.pin()` before returning, so the Arc keeps the pin alive.
        let cached_page = match get_or_fill_page(inode, *page_idx) {
            Ok(p) => p,
            Err(_) => {
                return FaultOutcome {
                    mapped: false,
                    cached_page: None,
                };
            }
        };

        let frame = cached_page.frame();

        // Bump the frame refcount so COW handler's dec_refcount is balanced when it copies.
        frame_allocator().inc_refcount(frame);

        // Install the PTE (pt_flags already has COW_BIT set for private, stripped of WRITABLE).
        let success = unsafe { map_page_direct(cr3, page_addr, frame, info.pt_flags, phys_offset) };

        if success {
            x86_64::instructions::tlb::flush(page_addr);
            // MAP_SHARED: mark the page dirty unconditionally on first fault.
            // We can't observe individual stores, so we treat any mapped shared
            // page as potentially written. Cost: one extra writeback per
            // mapped-but-never-written page; acceptable until eviction is added.
            if *shared {
                inode.pages.mark_dirty(*page_idx);
                register_dirty_inode(inode);
            }
            FaultOutcome {
                mapped: true,
                cached_page: Some((*vma_page_slot, cached_page)),
            }
        } else if is_page_present(cr3, page_addr, phys_offset) {
            // Race: another CPU already mapped it; undo our refcount bump.
            frame_allocator().dec_refcount(frame);
            cached_page.unpin();
            FaultOutcome {
                mapped: true,
                cached_page: None,
            }
        } else {
            frame_allocator().dec_refcount(frame);
            cached_page.unpin();
            FaultOutcome {
                mapped: false,
                cached_page: None,
            }
        }
    } else {
        // --- Zero / ElfData path (original logic) ---

        // Allocate a physical frame
        let frame = match frame_allocator().allocate_frame() {
            Some(f) => f,
            None => {
                return FaultOutcome {
                    mapped: false,
                    cached_page: None,
                };
            }
        };

        // Zero the frame via HHDM
        let frame_virt = phys_offset + frame.start_address().as_u64();
        unsafe {
            core::ptr::write_bytes(frame_virt.as_mut_ptr::<u8>(), 0, 4096);
        }

        // For ELF-backed pages, fill from the stored ELF data
        if let FaultSource::ElfData {
            elf_data,
            file_offset,
            file_size,
            vaddr_offset,
            vma_start,
        } = &info.source
        {
            let page_off_in_vma = page_addr.as_u64() - vma_start.as_u64();
            let seg_start = page_off_in_vma.saturating_sub(*vaddr_offset);
            let seg_end = (page_off_in_vma + 4096).saturating_sub(*vaddr_offset);
            let copy_start = seg_start.min(*file_size);
            let copy_end = seg_end.min(*file_size);

            if copy_end > copy_start {
                let elf_src_offset = (file_offset + copy_start) as usize;
                let elf_src_end = (file_offset + copy_end) as usize;
                if elf_src_end <= elf_data.len() {
                    let dst_offset = if page_off_in_vma < *vaddr_offset {
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
            FaultOutcome {
                mapped: true,
                cached_page: None,
            }
        } else {
            // Race: check if another CPU/path already mapped it
            if is_page_present(cr3, page_addr, phys_offset) {
                unsafe { frame_allocator().deallocate_frame(frame) };
                FaultOutcome {
                    mapped: true,
                    cached_page: None,
                }
            } else {
                unsafe { frame_allocator().deallocate_frame(frame) };
                FaultOutcome {
                    mapped: false,
                    cached_page: None,
                }
            }
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

    let outcome = fault_in_page(fault_addr, &fault_info, cr3_frame, phys_offset);

    // For FileBacked faults, store the Arc<CachedPage> on the VMA to keep it alive.
    if let Some((slot, cached_page)) = outcome.cached_page {
        // Re-acquire to mutate the VMA's pages slot.
        let thread2 = sched().current_thread();
        if let Some(t) = thread2 {
            if let Some(u) = &t.user {
                let user_read = u.read();
                let mut vmas = user_read.vmas.lock();
                let page_addr = fault_addr.align_down(4096u64);
                // Walk to find the VMA that covers the fault address and store the Arc.
                if let Some(vma) = vmas.find_mut(fault_addr) {
                    if let VmaBacking::FileBacked { pages, .. } = &mut vma.backing {
                        let _ = page_addr; // suppress unused warning
                        if slot < pages.len() {
                            pages[slot] = Some(cached_page);
                        }
                    }
                }
            }
        }
    }

    if outcome.mapped {
        if let Some(t) = sched().current_thread() {
            t.demand_faults
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
    outcome.mapped
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
        // Upgrade a pre-existing restrictive intermediate (e.g. installed by
        // a read-only SHM map via map_address) to have the permissive flags
        // we want. Safe because effective access is the AND across all levels
        // and the leaf enforces actual permissions.
        let new_flags = entry.flags() | flags;
        if new_flags != entry.flags() {
            entry.set_flags(new_flags);
        }
        Some(PhysFrame::containing_address(entry.addr()))
    } else {
        let new_frame = frame_allocator().allocate_frame()?;
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
