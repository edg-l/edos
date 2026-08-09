use alloc::sync::Arc;

use x86_64::{
    VirtAddr,
    registers::control::Cr3,
    structures::paging::{
        FrameAllocator, PageTable, PageTableFlags, PhysFrame, Size4KiB, page_table::PageTableEntry,
    },
};

use crate::{
    boot::boot_info,
    debug::lock_order::{RANK_USER_MM, RANK_VMAS},
    fs::{
        inode::VfsInode,
        page_cache::CachedPage,
        vfs::{fs_by_mount_id, get_or_fill_page, register_dirty_inode},
    },
    loader::reloc::RelocTable,
    memory::{
        frame_allocator::frame_allocator,
        mapper::COW_BIT,
        vma::{VmaBacking, VmaFlags, VmaProt, VmaSet},
    },
    ranked_lock,
};

use crate::thread::scheduler::current_thread;
use x86_64::structures::idt::PageFaultErrorCode;

/// Source of data for resolving a demand fault.
pub enum FaultSource {
    /// Zero-fill (Anonymous, Stack).
    Zero,
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

/// Why a demand fault declined to install a page.
///
/// A fault the handler refuses kills the faulting thread, and the address
/// alone does not say whether the mapping was missing, the permissions were
/// wrong, or the filesystem could not produce the page. The kill path prints
/// this so the log names the cause.
#[derive(Clone, Copy, Debug)]
pub enum FaultReject {
    /// A protection violation, which belongs to the COW handler.
    Protection,
    /// No current thread, or a kernel thread with no address space.
    NoUserThread,
    /// No VMA covers the address.
    NoVma,
    /// The VMA is not demand-faultable, or its backing is not supported.
    NotFaultable,
    /// Write to a VMA without `PROT_WRITE`.
    WriteToReadOnly,
    /// Instruction fetch from a VMA without `PROT_EXEC`.
    ExecOnNoExec,
    /// The mount backing the mapping is gone.
    MountGone,
    /// The page starts at or beyond EOF (SIGBUS-equivalent).
    PastEof { page_idx: u64, file_size: u64 },
    /// The filesystem could not fill the page.
    FillFailed { page_idx: u64 },
    /// Out of physical frames.
    NoFrame,
    /// The PTE could not be installed and the page is still not present.
    MapFailed,
    /// The lazy-relocation path could not produce the page.
    RelocFailed,
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
    /// Why the page was not mapped. `None` whenever `mapped` is true.
    pub reject: Option<FaultReject>,
    /// For FileBacked faults: the `Arc<CachedPage>` and VMA page-slot index that must be
    /// stored back into the VMA's `pages` vec.  The caller must re-acquire the VmaSet lock
    /// and call `vma.store_cached_page(slot, arc)` (or equivalent) to keep the Arc alive
    /// for the duration of the mapping.
    pub cached_page: Option<(usize, Arc<CachedPage>)>,
}

/// Fault in a single page given pre-extracted VMA info.
/// For Zero (Anonymous/Stack): allocates a zeroed frame and maps it.
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
        // Resolve the filesystem once for both the past-EOF check and
        // `get_or_fill_page`. If the mount vanished between mmap and this
        // fault, treat as EOF (kills the faulting thread).
        let fs = match fs_by_mount_id(inode.mount_id) {
            Some(f) => f,
            None => {
                return FaultOutcome {
                    mapped: false,
                    reject: Some(FaultReject::MountGone),
                    cached_page: None,
                };
            }
        };

        // Past-EOF check: if the faulting page starts at or beyond the file
        // size, return false (SIGBUS-equivalent — caller kills the thread).
        if let Ok(file_size) = fs.file_size_ino(inode.ino) {
            if page_idx * 4096 >= file_size {
                return FaultOutcome {
                    mapped: false,
                    reject: Some(FaultReject::PastEof {
                        page_idx: *page_idx,
                        file_size,
                    }),
                    cached_page: None,
                };
            }
        }

        // Get or fill the page from the inode page cache.
        // `get_or_fill_page` calls `page.pin()` before returning, so the Arc
        // keeps the pin alive.
        let cached_page = match get_or_fill_page(inode, *page_idx, &fs) {
            Ok(p) => p,
            Err(_) => {
                return FaultOutcome {
                    mapped: false,
                    reject: Some(FaultReject::FillFailed {
                        page_idx: *page_idx,
                    }),
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
                reject: None,
                cached_page: Some((*vma_page_slot, cached_page)),
            }
        } else if is_page_present(cr3, page_addr, phys_offset) {
            // Race: another CPU already mapped it; undo our refcount bump.
            frame_allocator().dec_refcount(frame);
            cached_page.unpin();
            FaultOutcome {
                mapped: true,
                reject: None,
                cached_page: None,
            }
        } else {
            frame_allocator().dec_refcount(frame);
            cached_page.unpin();
            FaultOutcome {
                mapped: false,
                reject: Some(FaultReject::MapFailed),
                cached_page: None,
            }
        }
    } else {
        // --- Zero path (Anonymous, Stack) ---

        // Allocate a physical frame
        let frame = match frame_allocator().allocate_frame() {
            Some(f) => f,
            None => {
                return FaultOutcome {
                    mapped: false,
                    reject: Some(FaultReject::NoFrame),
                    cached_page: None,
                };
            }
        };

        // Zero the frame via HHDM
        let frame_virt = phys_offset + frame.start_address().as_u64();
        unsafe {
            core::ptr::write_bytes(frame_virt.as_mut_ptr::<u8>(), 0, 4096);
        }

        // Map the page
        let success = unsafe { map_page_direct(cr3, page_addr, frame, info.pt_flags, phys_offset) };

        if success {
            x86_64::instructions::tlb::flush(page_addr);
            FaultOutcome {
                mapped: true,
                reject: None,
                cached_page: None,
            }
        } else {
            // Race: check if another CPU/path already mapped it
            if is_page_present(cr3, page_addr, phys_offset) {
                unsafe { frame_allocator().deallocate_frame(frame) };
                FaultOutcome {
                    mapped: true,
                    reject: None,
                    cached_page: None,
                }
            } else {
                unsafe { frame_allocator().deallocate_frame(frame) };
                FaultOutcome {
                    mapped: false,
                    reject: Some(FaultReject::MapFailed),
                    cached_page: None,
                }
            }
        }
    }
}

/// Handle a fault on a page in the reloc-bearing writable PT_LOAD VMA.
///
/// Allocates a fresh private frame, copies the cache page into it, applies
/// R_X86_64_RELATIVE relocations from `reloc_table`, and maps the frame
/// writable (no COW_BIT). The cache page is never patched.
///
/// Returns `Some(true)` on success, `Some(false)` on failure (caller kills
/// the faulting thread), or `None` if `fault_info` is not a FileBacked fault
/// (caller falls through to the normal demand path).
///
/// # Safety
/// Must be called from the page fault handler with the faulting thread's CR3 active.
unsafe fn fault_in_reloc_page(
    fault_addr: VirtAddr,
    fault_info: &FaultInfo,
    cr3: PhysFrame,
    phys_offset: VirtAddr,
    reloc_table: &RelocTable,
    page_vaddr_offset: u32,
    load_base: u64,
) -> Option<bool> {
    // Only applies to FileBacked MAP_PRIVATE faults.
    let (inode, page_idx) = match &fault_info.source {
        FaultSource::FileBacked {
            inode,
            page_idx,
            shared,
            ..
        } => {
            if *shared {
                // MAP_SHARED: no reloc patching needed (cache is the source of truth).
                return None;
            }
            (Arc::clone(inode), *page_idx)
        }
        // Non-FileBacked backing inside reloc_vma_range. Today this is only
        // the BSS-extension Anonymous VMA when BSS crosses a page boundary
        // past the FileBacked region — Phase 0 census confirmed no current
        // EDOS binary places reloc targets in such a page (last reloc on the
        // last file-data page, BSS extension begins past it). Returning None
        // lets the caller fall through to the normal demand path which
        // zero-fills the anon page. If a future binary violates this and
        // puts a reloc target in a BSS-extension page, the relocation will
        // silently NOT be applied and the program will crash visibly when
        // it dereferences the un-relocated zero pointer.
        _ => return None,
    };

    let page_addr = fault_addr.align_down(4096u64);

    let fs = match fs_by_mount_id(inode.mount_id) {
        Some(f) => f,
        None => return Some(false),
    };

    // Past-EOF check: same as normal FileBacked path.
    if let Ok(file_size) = fs.file_size_ino(inode.ino) {
        if page_idx * 4096 >= file_size {
            return Some(false);
        }
    }

    // Get the cache page (pins it).
    let cached_page = match get_or_fill_page(&inode, page_idx, &fs) {
        Ok(p) => p,
        Err(_) => return Some(false),
    };

    let cache_frame = cached_page.frame();

    // Allocate a fresh private frame (never shared with the cache).
    let private_frame = match frame_allocator().allocate_frame() {
        Some(f) => f,
        None => {
            cached_page.unpin();
            return Some(false);
        }
    };

    // Copy the cache page into the private frame via HHDM.
    let cache_ptr = (phys_offset + cache_frame.start_address().as_u64()).as_ptr::<u8>();
    let private_ptr = (phys_offset + private_frame.start_address().as_u64()).as_mut_ptr::<u8>();
    unsafe {
        core::ptr::copy_nonoverlapping(cache_ptr, private_ptr, 4096);
    }

    // Release the cache pin now that we have our own copy.
    cached_page.unpin();
    drop(cached_page);

    // Apply all RELATIVE relocations that land in this page.
    unsafe {
        reloc_table.apply_relocs_to_page(
            phys_offset + private_frame.start_address().as_u64(),
            page_vaddr_offset,
            load_base,
        );
    }

    // Build PTE flags: writable, no COW_BIT (this is our private patched copy).
    let mut pt_flags = fault_info.pt_flags;
    // Strip COW_BIT (set by lookup_fault_vma for MAP_PRIVATE) and add WRITABLE.
    pt_flags &= !COW_BIT;
    pt_flags |= PageTableFlags::WRITABLE;

    // Map the private frame at the faulting page address.
    let success = unsafe { map_page_direct(cr3, page_addr, private_frame, pt_flags, phys_offset) };

    if success {
        x86_64::instructions::tlb::flush(page_addr);
        Some(true)
    } else if is_page_present(cr3, page_addr, phys_offset) {
        // Race: another CPU already mapped it; free our private frame.
        unsafe { frame_allocator().deallocate_frame(private_frame) };
        Some(true)
    } else {
        unsafe { frame_allocator().deallocate_frame(private_frame) };
        Some(false)
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
pub unsafe fn handle_demand_fault(
    fault_addr: VirtAddr,
    error_code: PageFaultErrorCode,
) -> Result<(), FaultReject> {
    // Don't handle protection violations here (those go to COW handler)
    if error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION) {
        return Err(FaultReject::Protection);
    }

    // Get current thread's VmaSet
    let thread = match current_thread() {
        Some(t) => t,
        None => return Err(FaultReject::NoUserThread),
    };
    let user = match &thread.user {
        Some(u) => u,
        None => return Err(FaultReject::NoUserThread),
    };

    // Read the VmaSet under spin lock (IST-safe)
    let user_read = user.read();
    let vmas = ranked_lock!(RANK_VMAS, "user.vmas", user_read.vmas);

    let vma = match vmas.find(fault_addr) {
        Some(v) => v,
        None => return Err(FaultReject::NoVma),
    };

    // Permission checks from the hardware error code
    let is_write = error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE);
    if is_write && !vma.prot.contains(VmaProt::WRITE) {
        return Err(FaultReject::WriteToReadOnly);
    }
    let is_exec = error_code.contains(PageFaultErrorCode::INSTRUCTION_FETCH);
    if is_exec && !vma.prot.contains(VmaProt::EXEC) {
        return Err(FaultReject::ExecOnNoExec);
    }

    let fault_info = match lookup_fault_vma(&vmas, fault_addr) {
        Some(info) => info,
        None => return Err(FaultReject::NotFaultable),
    };

    // Reloc-page early path: if this fault hits the writable PT_LOAD VMA and
    // the process has a RelocTable, allocate a fresh private frame, copy from
    // the cache page, apply relocations, and map it writable immediately
    // (no COW_BIT). The cache page is never patched.
    let reloc_info: Option<(Arc<RelocTable>, u32, u64)> = {
        let mm = ranked_lock!(RANK_USER_MM, "user.mm", user_read.memory_manager);
        if let (Some(table), Some(range)) = (&mm.reloc_table, &mm.reloc_vma_range) {
            let page_addr = fault_addr.align_down(4096u64);
            if range.contains(&page_addr) {
                // page_vaddr_offset: page-aligned byte offset from load base.
                let page_vaddr_offset = (page_addr.as_u64() - mm.load_base) as u32;
                Some((Arc::clone(table), page_vaddr_offset, mm.load_base))
            } else {
                None
            }
        } else {
            None
        }
    };

    // Drop locks before allocating (frame_allocator uses its own lock)
    drop(vmas);
    drop(user_read);

    let (cr3_frame, _) = Cr3::read();
    let phys_offset = boot_info().physical_memory_offset;

    // If this is a reloc-bearing page, handle it with the lazy reloc path.
    if let Some((reloc_table, page_vaddr_offset, load_base)) = reloc_info {
        if let Some(mapped) = unsafe {
            fault_in_reloc_page(
                fault_addr,
                &fault_info,
                cr3_frame,
                phys_offset,
                &reloc_table,
                page_vaddr_offset,
                load_base,
            )
        } {
            if !mapped {
                return Err(FaultReject::RelocFailed);
            }
            if let Some(t) = current_thread() {
                t.demand_faults
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            return Ok(());
        }
        // Fall through to normal path if reloc path declines (e.g. not FileBacked).
    }

    let outcome = fault_in_page(fault_addr, &fault_info, cr3_frame, phys_offset);

    // For FileBacked faults, store the Arc<CachedPage> on the VMA to keep it
    // alive. We released the vmas lock across fault_in_page so a concurrent
    // split_at/remove_range may have altered the VMA layout. Recompute the
    // slot from the CURRENT VMA's start+file_offset instead of trusting the
    // pre-drop slot index.
    //
    // If the VMA is gone (remove_range), sys_munmap already unmapped the PTE
    // and dec_refcount'd our bump, so we just drop the Arc here (unpin is
    // the only leftover side effect).
    if let Some((_original_slot, cached_page)) = outcome.cached_page {
        let _ = store_cached_page_on_vma(fault_addr, cached_page);
    }

    if !outcome.mapped {
        return Err(outcome.reject.unwrap_or(FaultReject::MapFailed));
    }
    if let Some(t) = current_thread() {
        t.demand_faults
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

/// Re-acquire the current thread's VmaSet lock, find the FileBacked VMA that
/// contains `fault_addr`, recompute the per-VMA page slot from CURRENT bounds
/// (not a stale slot index captured before the lock was dropped), and store
/// the `Arc<CachedPage>` there. Returns true if stored.
///
/// If the VMA has been split/removed between fault-in and this call, the
/// caller passes through: a concurrent sys_munmap also dec_refcount'd the
/// fault-in bump when it tore down the PTE, so dropping the Arc here is
/// sufficient cleanup.
pub fn store_cached_page_on_vma(
    fault_addr: VirtAddr,
    cached_page: Arc<crate::fs::page_cache::CachedPage>,
) -> bool {
    // `cached_page` carries a +1 pin from `get_or_fill_page`. Once this
    // function consumes it, the pin's purpose is over -- either the Arc
    // ends up stored in the VMA (Arc keeps the page alive) or it drops
    // here (last ref, frame freed via `CachedPage::drop`). Unpin up front
    // so every return path sees pin_count=0 at Arc drop time.
    cached_page.unpin();

    let t = match current_thread() {
        Some(t) => t,
        None => return false,
    };
    let u = match &t.user {
        Some(u) => u,
        None => return false,
    };
    let user_read = u.read();
    let mut vmas = ranked_lock!(RANK_VMAS, "user.vmas", user_read.vmas);
    let vma = match vmas.find_mut(fault_addr) {
        Some(v) => v,
        None => return false,
    };
    let vma_start = vma.start.as_u64();
    let page_addr = fault_addr.align_down(4096u64).as_u64();
    if page_addr < vma_start {
        return false;
    }
    let slot = ((page_addr - vma_start) / 4096) as usize;
    if let VmaBacking::FileBacked { pages, .. } = &mut vma.backing {
        if slot < pages.len() {
            pages[slot] = Some(cached_page);
            return true;
        }
    }
    false
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

    #[cfg(debug_assertions)]
    crate::memory::mapper::debug_assert_permissive_parents(cr3, vaddr);

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
