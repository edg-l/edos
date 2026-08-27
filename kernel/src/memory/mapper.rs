use crate::thread::preempt::PreemptSpinlock;
use alloc::sync::Arc;
use core::ops::Range;
use x86_64::{
    PhysAddr, VirtAddr,
    registers::control::Cr3Flags,
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags,
        PhysFrame, Size4KiB, Translate,
        mapper::{CleanUp, FlagUpdateError, MapToError, TranslateResult, UnmapError},
        page::PageRangeInclusive,
    },
};

use crate::{
    boot::boot_info,
    debug::lock_order::{RANK_KERNEL_MAPPER, RankedGuard},
    loader::reloc::RelocTable,
    memory::{
        STACK_ALIGNMENT,
        frame_allocator::frame_allocator,
        vma::{USER_VA_END, VmaSet},
    },
    thread::irqlock::IrqLockGuard,
};

/// OS-available PTE bit used to mark copy-on-write pages.
pub const COW_BIT: PageTableFlags = PageTableFlags::BIT_9;

/// Entries in one page table at any level.
const ENTRIES_PER_TABLE: u64 = 512;

/// The live PML4, as named by CR3.
///
/// # Safety
///
/// `physical_memory_offset` must be the base of a complete mapping of physical
/// memory. The returned reference is `'static` and exclusive, so the caller
/// must not let a second one to the same table exist, and must hold the address
/// space against concurrent modification for as long as it is used.
pub unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_table_frame, _) = Cr3::read();

    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    // SAFETY: CR3 holds the physical address of the live PML4, and the physical
    // offset mapping covers all of RAM at page alignment. Exclusivity is the
    // caller's, per this function's # Safety section.
    unsafe { &mut *page_table_ptr }
}

/// The PML4 named by an arbitrary `cr3` value, live or not.
///
/// # Safety
///
/// `cr3.0` must name a page table frame that is still allocated. The same
/// exclusivity contract as [`active_level_4_table`] applies to the result.
pub unsafe fn get_level_4_table(cr3: (PhysFrame, Cr3Flags)) -> &'static mut PageTable {
    let physical_memory_offset = boot_info().physical_memory_offset;

    let phys = cr3.0.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    // SAFETY: cr3.0 names a PML4 frame, mapped through the physical offset like
    // any other RAM. Exclusivity is the caller's, per the # Safety section.
    unsafe { &mut *page_table_ptr }
}

/// Acquire the kernel-global memory mapper (rank 85).
/// Returns a `RankedGuard` that pops the rank stack on drop, after releasing
/// the `IrqLockGuard`. Zero-cost in release builds.
pub fn memory_mapper() -> RankedGuard<IrqLockGuard<'static, MemoryManager>> {
    boot_info()
        .memory_manager
        .lock_ranked(RANK_KERNEL_MAPPER, "kernel.mapper")
}

#[derive(Debug)]
pub struct MemoryManager {
    pub mapper: OffsetPageTable<'static>,
    /// PML4 physical frame (user processes only, None for kernel mapper)
    pub pml4_frame: Option<PhysFrame>,
    /// VMA set (user processes only, None for kernel mapper)
    pub vmas: Option<Arc<PreemptSpinlock<VmaSet>>>,
    /// Parsed R_X86_64_RELATIVE table for lazy page-fault relocation application.
    /// Shared (Arc) so fork can clone it cheaply without re-parsing.
    pub reloc_table: Option<Arc<RelocTable>>,
    /// Load-base-relative virtual address range of the writable PT_LOAD VMA
    /// that contains reloc targets. Used by the fault handler to decide whether
    /// to apply relocs when faulting a private writable page.
    pub reloc_vma_range: Option<Range<VirtAddr>>,
    /// ELF load base for this process (used by the fault handler to compute
    /// relocated values: `value = load_base + entry.addend`).
    pub load_base: u64,
    /// Set once the root page-table frame has been handed back to the frame
    /// allocator, after which `mapper` points at memory that may already have
    /// been reused. Nothing may follow it from that moment; see
    /// [`MemoryManager::release_page_tables`].
    released: bool,
}

/// The kernel half starts here: every address at or above it is kernel-owned
/// and mapped identically in every address space, which is exactly what the
/// `GLOBAL` bit asserts.
const KERNEL_HALF_START: u64 = 0xFFFF_8000_0000_0000;

/// Add `GLOBAL` to a kernel-half mapping, so a `CR3` reload keeps it.
///
/// `mark_kernel_mappings_global` sweeps the kernel half once at boot, and
/// anything mapped after that sweep would otherwise be non-global -- which
/// covered the two regions every syscall and every switch touch: a thread's
/// kernel stack (`kthread_stack_alloc`) and the per-CPU scheduler stack the
/// voluntary switch pivots onto. Doing it here rather than at the call sites is
/// what stops the next kernel-half mapping from forgetting.
///
/// Unmapping a global entry needs explicit invalidation, and it gets it:
/// `Mapper::unmap`'s flush is an `invlpg`, which ignores the `G` bit, and
/// `tlb_shootdown` either issues `invlpg` per page or toggles `CR4.PGE`.
fn with_global_if_kernel(addr: VirtAddr, flags: PageTableFlags) -> PageTableFlags {
    if addr.as_u64() >= KERNEL_HALF_START && !flags.contains(PageTableFlags::USER_ACCESSIBLE) {
        flags | PageTableFlags::GLOBAL
    } else {
        flags
    }
}

#[expect(
    unused,
    reason = "the mapper's diagnostic half: walks and dumps no path calls today"
)]
impl MemoryManager {
    pub fn new(page_table: OffsetPageTable<'static>) -> Self {
        Self {
            mapper: page_table,
            pml4_frame: None,
            vmas: None,
            reloc_table: None,
            reloc_vma_range: None,
            load_base: 0,
            released: false,
        }
    }

    /// Mark this address space's page tables as gone, so nothing reads them
    /// after the root frame goes back to the allocator.
    ///
    /// `mapper` is an `OffsetPageTable<'static>` over a frame this manager does
    /// not own, and teardown frees that frame while the manager itself lives on
    /// behind an `Arc` that procfs and any other observer may still hold. The
    /// lifetime says nothing about that, so the flag has to.
    pub fn release_page_tables(&mut self) {
        self.released = true;
        self.pml4_frame = None;
    }

    /// Maps memory, the default flag is PRESENT, use extra flags for more.
    ///
    /// A kernel-half mapping is made `GLOBAL`; see [`with_global_if_kernel`].
    pub fn map_memory(
        &mut self,
        addr: VirtAddr,
        size: u64,
        extra_flags: PageTableFlags,
    ) -> Result<PageRangeInclusive<Size4KiB>, MapToError<Size4KiB>> {
        let page_range = get_page_range(addr, size);

        let flags = with_global_if_kernel(addr, PageTableFlags::PRESENT | extra_flags);
        {
            let mut frame_allocator = frame_allocator();

            for page in page_range {
                let frame = frame_allocator
                    .allocate_frame()
                    .ok_or(MapToError::FrameAllocationFailed)?;
                // SAFETY: page is unmapped in this address space (the range was checked
                // above), and frame was just allocated so nothing else refers to it. The
                // flush covers the entry map_to created.
                unsafe {
                    self.mapper
                        .map_to(page, frame, flags, &mut **frame_allocator)?
                        .flush()
                };
            }
        }

        Ok(page_range)
    }

    /// Maps memory, the default flag is PRESENT, use extra flags for more.
    pub fn map_memory_contiguous(
        &mut self,
        addr: VirtAddr,
        size: u64,
        extra_flags: PageTableFlags,
    ) -> Result<PageRangeInclusive<Size4KiB>, MapToError<Size4KiB>> {
        let page_range = get_page_range(addr, size);

        let flags = with_global_if_kernel(addr, PageTableFlags::PRESENT | extra_flags);
        {
            let mut frame_allocator = frame_allocator();
            let frame = frame_allocator
                .allocate_contiguous_frames(page_range.count())
                .ok_or(MapToError::FrameAllocationFailed)?;

            for (i, page) in page_range.enumerate() {
                let current_frame = PhysFrame::containing_address(
                    frame.start_address() + (i as u64 * Size4KiB::SIZE),
                );
                // SAFETY: each page in the range is unmapped in this address space, and the
                // frames come from one contiguous allocation owned by this mapping alone.
                unsafe {
                    self.mapper
                        .map_to(page, current_frame, flags, &mut **frame_allocator)?
                        .flush()
                };
            }
        }

        Ok(page_range)
    }

    /// Change page table flags for a range. The local TLB is flushed inline.
    ///
    /// If the change reduces permissions (e.g., removing WRITABLE), the caller
    /// must issue a TLB shootdown after this call returns (and after dropping
    /// the mapper lock) to ensure other CPUs see the permission change.
    pub fn change_flags(
        &mut self,
        addr: VirtAddr,
        size: u64,
        flags: PageTableFlags,
    ) -> Result<(), FlagUpdateError> {
        let page_range = get_page_range(addr, size);

        for page in page_range {
            // SAFETY: changing flags on an already-present entry cannot invalidate a
            // frame's ownership, and the flush that follows retires any TLB entry
            // cached under the old flags.
            unsafe {
                self.mapper
                    .update_flags(page, PageTableFlags::PRESENT | flags)?
                    .flush();
            }
        }
        Ok(())
    }

    pub fn unmap_memory(
        &mut self,
        addr: VirtAddr,
        size: u64,
    ) -> Result<PageRangeInclusive<Size4KiB>, UnmapError> {
        let page_range = get_page_range(addr, size);
        let mut frame_allocator = frame_allocator();

        for page in page_range {
            // Get the frame before unmapping so we can deallocate it
            if let Ok(frame) = self.mapper.translate_page(page) {
                // Unmap the page
                let (_, flush) = self.mapper.unmap(page)?;
                flush.flush();

                // Reclaim the frame to the allocator
                // SAFETY: the page was mapped to frame and has just been unmapped and
                // flushed, so no translation to it survives. Only privately owned
                // anonymous mappings reach this path, so the frame has one owner.
                unsafe {
                    frame_allocator.deallocate_frame(frame);
                }
            } else {
                // Page wasn't mapped, still try to unmap to clear any stale entries
                let (_, flush) = self.mapper.unmap(page)?;
                flush.flush();
            }
        }

        Ok(page_range)
    }

    /// Tear down a mapping onto physical memory the frame allocator does not
    /// own: firmware tables, MMIO windows, anything the caller borrowed by
    /// physical address rather than allocated.
    ///
    /// Such a frame is either outside the bitmap entirely or marked allocated
    /// with a refcount of zero, which is the allocator's "reserved, never
    /// hand this out" state. `unmap_memory` would clear that bit and put
    /// firmware or device pages back in the free pool, so it must not be used
    /// here.
    pub fn unmap_foreign_memory(
        &mut self,
        addr: VirtAddr,
        size: u64,
    ) -> Result<PageRangeInclusive<Size4KiB>, UnmapError> {
        let page_range = get_page_range(addr, size);

        for page in page_range {
            let (_, flush) = self.mapper.unmap(page)?;
            flush.flush();
        }

        Ok(page_range)
    }

    /// Unmap pages and return the freed frames WITHOUT deallocating them.
    /// The caller must free the frames after completing a TLB shootdown.
    pub fn unmap_memory_deferred(
        &mut self,
        addr: VirtAddr,
        size: u64,
    ) -> Result<alloc::vec::Vec<PhysFrame>, UnmapError> {
        let page_range = get_page_range(addr, size);
        let mut freed_frames = alloc::vec::Vec::new();

        for page in page_range {
            if let Ok(frame) = self.mapper.translate_page(page) {
                let (_, flush) = self.mapper.unmap(page)?;
                flush.flush();
                freed_frames.push(frame);
            } else {
                let (_, flush) = self.mapper.unmap(page)?;
                flush.flush();
            }
        }

        Ok(freed_frames)
    }

    /// Map a given physical address.
    ///
    /// Intermediate page-table entries (PML4/PML3/PML2) are installed and
    /// upgraded as needed to be `PRESENT | WRITABLE | USER_ACCESSIBLE` so a
    /// restrictive earlier mapping (e.g. read-only SHM) does not poison the
    /// range for later writable mappings. x86-64 effective access is the AND
    /// across all levels; the leaf flags control effective permissions.
    pub fn map_address(
        &mut self,
        virt_addr: VirtAddr,
        phys_addr: PhysAddr,
        flags: PageTableFlags,
    ) -> Result<(), x86_64::structures::paging::mapper::MapToError<Size4KiB>> {
        // Diagnostic: log map_address calls targeting the first 1 MiB of RAM.
        // Legitimate MMIO is at high phys; legitimate user/loader mappings are
        // to anonymous phys that the allocator handed out post-heap. Any low
        // phys here is a strong smell for the heap-alias bug.
        if phys_addr.as_u64() < 0x0010_0000 {
            crate::println!(
                "map_address LOW-PHYS: virt={:#x} phys={:#x} flags={:?}",
                virt_addr.as_u64(),
                phys_addr.as_u64(),
                flags
            );
        }
        let page = Page::containing_address(virt_addr);
        let frame = PhysFrame::containing_address(phys_addr);
        let mut frame_allocator = frame_allocator();

        let parent_flags =
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;

        // Upgrade any existing restrictive intermediate entries. Use pml4_frame
        // for user MMs; fall back to current CR3 for the kernel global mapper.
        let pml4_frame = self.pml4_frame.unwrap_or_else(|| {
            use x86_64::registers::control::Cr3;
            Cr3::read().0
        });
        let phys_off = boot_info().physical_memory_offset;
        let pml4_ptr = (phys_off + pml4_frame.start_address().as_u64()).as_mut_ptr::<PageTable>();
        // SAFETY: pml4_ptr addresses a live PML4 through the physical offset
        // mapping, and the mapper lock this method holds keeps any other CPU out
        // of the same tables.
        unsafe { upgrade_parent_entries(&mut *pml4_ptr, virt_addr, parent_flags) };

        // SAFETY: page is either unmapped or being remapped to frame by an explicit
        // request; the parent entries were just upgraded to cover the new leaf.
        unsafe {
            self.mapper
                .map_to_with_table_flags(
                    page,
                    frame,
                    PageTableFlags::PRESENT | flags,
                    parent_flags,
                    &mut **frame_allocator,
                )?
                .flush()
        };

        // The caller supplied the physical frame (MMIO, firmware, shared
        // memory), so the allocator must never hand it out, and must never
        // free it either: reserve it with a refcount of 0 rather than taking
        // a reference. Tearing such a mapping down is `unmap_foreign_memory`.
        if let Some(idx) = frame_allocator.frame_to_index(frame) {
            frame_allocator.set_frame_allocated(idx);
        }

        #[cfg(debug_assertions)]
        debug_assert_permissive_parents(pml4_frame, virt_addr);

        Ok(())
    }

    /// Map a range of virtual addresses to physical addresses
    ///
    /// Returns the total offset from the base addr.
    pub fn map_address_range(
        &mut self,
        virt_start: VirtAddr,
        phys_start: PhysAddr,
        size: usize,
        flags: PageTableFlags,
    ) -> Result<u64, x86_64::structures::paging::mapper::MapToError<Size4KiB>> {
        let page_count = size.div_ceil(4096);

        let mut virt_addr = VirtAddr::new(virt_start.as_u64());
        let mut phys_addr = PhysAddr::new(phys_start.as_u64());
        let mut offset = 0;

        for i in 0..page_count {
            offset = i as u64 * 4096;
            virt_addr = VirtAddr::new(virt_start.as_u64() + offset);
            phys_addr = PhysAddr::new(phys_start.as_u64() + offset);

            self.map_address(virt_addr, phys_addr, flags)?;
        }

        Ok(offset)
    }

    /// Return the frame that the given virtual address is mapped to and the offset within that frame.
    ///
    /// If the given address has a valid mapping, the mapped frame and the offset within that frame is returned. Otherwise an error value is returned.
    ///
    /// This function works with huge pages of all sizes.
    pub fn translate(&self, addr: VirtAddr) -> TranslateResult {
        self.mapper.translate(addr)
    }

    /// Bytes actually mapped into the user half of this address space.
    ///
    /// Counted from the page tables rather than tracked in a field, because a
    /// page reaches a user address space from demand paging, copy-on-write,
    /// `mmap`, shared memory and the loader, and leaves it from as many places
    /// again; a counter maintained at each of those drifts the first time one
    /// is missed, and a drifting number is worse than none. The walk descends
    /// only into present entries, so the lazily faulted mappings this kernel
    /// leans on -- a whole ELF image, a grown stack -- cost one skipped entry
    /// each rather than a probe per page.
    ///
    /// A page shared with another address space is counted in both, as
    /// `/proc/<pid>/status` on Linux counts it.
    pub fn resident_bytes(&self) -> u64 {
        self.resident_bytes_in(0, USER_VA_END)
    }

    /// Bytes of `[start, end)` mapped into this address space.
    ///
    /// The same walk as [`resident_bytes`] clipped to one range, which is what
    /// `/proc/<tid>/maps` reports per mapping: a VMA is only a request, and the
    /// gap between its size and this number is the demand paging that has not
    /// happened yet.
    ///
    /// Raw addresses rather than `VirtAddr`, because the exclusive end of the
    /// user half is [`USER_VA_END`], which is the lowest *non*-canonical
    /// address and cannot be held in one.
    pub fn resident_bytes_in(&self, start: u64, end: u64) -> u64 {
        // The tables are gone; following `mapper` now would read a recycled
        // frame and take its bytes for page-table entries.
        if self.released || end <= start {
            return 0;
        }
        let phys_off = boot_info().physical_memory_offset;
        // The user half is the low PML4 entries, one per 512 GiB.
        const PML4_ENTRY_SPAN: u64 = 512 * 1024 * 1024 * 1024;
        self.mapper
            .level_4_table()
            .iter()
            .enumerate()
            .take((USER_VA_END / PML4_ENTRY_SPAN) as usize)
            .map(|(index, entry)| {
                let base = index as u64 * PML4_ENTRY_SPAN;
                if base >= end
                    || base + PML4_ENTRY_SPAN <= start
                    || !entry.flags().contains(PageTableFlags::PRESENT)
                {
                    return 0;
                }
                // SAFETY: entry is PRESENT and not a leaf, so its address names a page table
                // frame reachable through the physical offset mapping. Read-only for the
                // length of the count.
                let pdpt = unsafe { &*(phys_off + entry.addr().as_u64()).as_ptr::<PageTable>() };
                count_present_bytes(
                    pdpt,
                    PML4_ENTRY_SPAN / ENTRIES_PER_TABLE,
                    base,
                    phys_off,
                    start,
                    end,
                )
            })
            .sum()
    }

    /// Translate a virtual address mapped in this page table to a kernel HHDM pointer.
    /// If the page is not yet present and this MemoryManager has an attached VmaSet,
    /// demand-faults the page before returning the pointer.
    fn translate_to_hhdm_ptr(&self, vaddr: VirtAddr) -> Option<*mut u8> {
        // Fast path: page already mapped
        if let TranslateResult::Mapped { frame, offset, .. } = self.mapper.translate(vaddr) {
            let phys = frame.start_address() + offset;
            let hhdm = crate::boot::boot_info().physical_memory_offset;
            return Some((hhdm + phys.as_u64()).as_mut_ptr());
        }

        // Slow path: demand-fault via attached VmaSet. Acquires rank-70 vmas. Callers that
        // hold rank-80 mm MUST ensure the page is already mapped (e.g. via eager map_memory)
        // so this branch is not taken, otherwise the rank tracker fires (80 -> 70 inversion).
        // See doc/invariants/lock-order.md rank-80 note.
        let vmas_arc = self.vmas.as_ref()?;
        let pml4 = self.pml4_frame?;
        let phys_offset = crate::boot::boot_info().physical_memory_offset;

        let fault_info = {
            let vmas = vmas_arc.lock();
            crate::memory::fault::lookup_fault_vma(&vmas, vaddr)?
        };
        // VmaSet lock dropped before allocating frames

        let outcome = crate::memory::fault::fault_in_page(vaddr, &fault_info, pml4, phys_offset);

        // For FileBacked faults, store the Arc<CachedPage> on the VMA,
        // recomputing the slot from the CURRENT VMA bounds (the pre-drop
        // slot index is stale if a concurrent split_at ran).
        if let Some((_original_slot, cached_page)) = outcome.cached_page {
            let mut vmas = vmas_arc.lock();
            if let Some(vma) = vmas.find_mut(vaddr) {
                let vma_start = vma.start.as_u64();
                let page_addr = vaddr.align_down(4096u64).as_u64();
                if page_addr >= vma_start {
                    let slot = ((page_addr - vma_start) / 4096) as usize;
                    if let crate::memory::vma::VmaBacking::FileBacked { pages, .. } =
                        &mut vma.backing
                        && slot < pages.len()
                    {
                        pages[slot] = Some(cached_page);
                    }
                }
            }
        }

        if outcome.mapped {
            // Retry translation after mapping
            if let TranslateResult::Mapped { frame, offset, .. } = self.mapper.translate(vaddr) {
                let phys = frame.start_address() + offset;
                return Some((phys_offset + phys.as_u64()).as_mut_ptr());
            }
        }

        None
    }

    /// Copy bytes into user virtual address space via HHDM, handling page boundaries.
    pub fn copy_to_user(&self, dest_vaddr: VirtAddr, src: &[u8]) {
        let mut offset = 0usize;
        while offset < src.len() {
            let current_vaddr = dest_vaddr + offset as u64;
            let page_offset = (current_vaddr.as_u64() & 0xFFF) as usize;
            let chunk = (4096 - page_offset).min(src.len() - offset);
            let dest_ptr = self
                .translate_to_hhdm_ptr(current_vaddr)
                .expect("copy_to_user: page not mapped");
            // SAFETY: dest_ptr is the HHDM alias of a mapped user page, so writable, and
            // chunk is clamped to the remainder of that page. src is a live slice and
            // the HHDM alias of a user frame cannot overlap it.
            unsafe {
                core::ptr::copy_nonoverlapping(src[offset..].as_ptr(), dest_ptr, chunk);
            }
            offset += chunk;
        }
    }

    /// Zero bytes in user virtual address space via HHDM, handling page boundaries.
    pub fn zero_user(&self, dest_vaddr: VirtAddr, len: usize) {
        let mut offset = 0usize;
        while offset < len {
            let current_vaddr = dest_vaddr + offset as u64;
            let page_offset = (current_vaddr.as_u64() & 0xFFF) as usize;
            let chunk = (4096 - page_offset).min(len - offset);
            let dest_ptr = self
                .translate_to_hhdm_ptr(current_vaddr)
                .expect("zero_user: page not mapped");
            // SAFETY: dest_ptr is the HHDM alias of a mapped user page and chunk is
            // clamped to the remainder of that page.
            unsafe {
                core::ptr::write_bytes(dest_ptr, 0, chunk);
            }
            offset += chunk;
        }
    }

    /// Write a value to user virtual address space via HHDM.
    /// Uses copy_to_user internally so it handles page boundaries safely.
    pub fn write_val_to_user<T: Copy>(&self, dest_vaddr: VirtAddr, value: T) {
        // SAFETY: T: Copy has no padding invariant to violate when read as bytes,
        // and the slice borrows `value`, which outlives the copy_to_user call
        // below.
        let bytes = unsafe {
            core::slice::from_raw_parts(&value as *const T as *const u8, core::mem::size_of::<T>())
        };
        self.copy_to_user(dest_vaddr, bytes);
    }

    pub fn clean_lower_half(&mut self) {
        let lower_half = Page::range_inclusive(
            Page::containing_address(VirtAddr::new(0)),
            Page::containing_address(VirtAddr::new(0x0000_7fff_ffff_ffff)),
        );

        // SAFETY: the lower half belongs to this address space alone and every
        // mapping in it has already been unmapped, so the tables clean_up frees
        // are unreachable from any CR3 but this one.
        unsafe {
            self.mapper
                .clean_up_addr_range(lower_half, &mut **frame_allocator())
        };
    }
}

pub fn get_page_range(addr: VirtAddr, size: u64) -> PageRangeInclusive<Size4KiB> {
    let start_page = Page::containing_address(addr);

    // Calculate the address of the last byte
    let end_addr = VirtAddr::new(addr.as_u64() + size - 1);
    let end_page = Page::containing_address(end_addr);

    // Create an inclusive range of pages.
    Page::range_inclusive(start_page, end_page)
}

/// Align a stack pointer down to the required stack alignment (16 bytes for FPU/SSE)
pub fn align_stack_pointer(stack_ptr: VirtAddr) -> VirtAddr {
    VirtAddr::new(stack_ptr.as_u64() & !(STACK_ALIGNMENT - 1))
}

/// Bytes of `[start, end)` mapped by `table`, which covers `[base, base +
/// 512 * entry_span)`: a PDPT spans 1 GiB per entry, a PD 2 MiB, a page table
/// 4 KiB. Used by [`MemoryManager::resident_bytes_in`].
fn count_present_bytes(
    table: &PageTable,
    entry_span: u64,
    base: u64,
    phys_off: VirtAddr,
    start: u64,
    end: u64,
) -> u64 {
    table
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let entry_base = base + index as u64 * entry_span;
            let entry_end = entry_base + entry_span;
            if entry_base >= end
                || entry_end <= start
                || !entry.flags().contains(PageTableFlags::PRESENT)
            {
                return 0;
            }
            if entry_span == Size4KiB::SIZE || entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                // A 2 MiB leaf can straddle either end of the range, so count
                // only the part asked for.
                return entry_end.min(end) - entry_base.max(start);
            }
            // SAFETY: entry is PRESENT and not a leaf, so it addresses a page table
            // frame, reachable through the physical offset mapping.
            let child = unsafe { &*(phys_off + entry.addr().as_u64()).as_ptr::<PageTable>() };
            count_present_bytes(
                child,
                entry_span / ENTRIES_PER_TABLE,
                entry_base,
                phys_off,
                start,
                end,
            )
        })
        .sum()
}

/// Walk the page-table chain from `pml4` down to (but not including) the PML1
/// leaf for `virt_addr`, OR-ing `flags` into every present intermediate entry.
/// Does not create new entries (that is the mapper's job).  Required because
/// `map_to_with_table_flags` only applies parent flags to NEWLY created
/// intermediates, not existing ones.  Without this, a restrictive earlier
/// mapping (e.g. read-only SHM) leaves PML2/3/4 entries without WRITABLE, and
/// x86-64's AND-across-levels semantics then block any later writable leaf
/// in the same 2 MiB / 1 GiB / 512 GiB range.
///
/// # Safety
///
/// `pml4` must be the live top-level table for the address space `virt_addr`
/// belongs to, and the caller must hold that address space against concurrent
/// modification -- the mapper lock, for the paths that reach this.
unsafe fn upgrade_parent_entries(pml4: &mut PageTable, virt_addr: VirtAddr, flags: PageTableFlags) {
    let a = virt_addr.as_u64();
    let i4 = ((a >> 39) & 0x1FF) as usize;
    let i3 = ((a >> 30) & 0x1FF) as usize;
    let i2 = ((a >> 21) & 0x1FF) as usize;
    let phys_off = boot_info().physical_memory_offset;
    let pt_from_frame = |f: PhysFrame| -> &'static mut PageTable {
        let virt = phys_off + f.start_address().as_u64();
        // SAFETY: f names a page table frame reached by walking the live hierarchy,
        // mapped through the physical offset. upgrade_parent_entries' caller holds
        // the mapper lock, so this is the only walker.
        unsafe { &mut *virt.as_mut_ptr::<PageTable>() }
    };

    let p4e = &mut pml4[i4];
    if p4e.flags().contains(PageTableFlags::PRESENT) {
        let new_flags = p4e.flags() | flags;
        if new_flags != p4e.flags() {
            p4e.set_flags(new_flags);
        }
        let pml3 = pt_from_frame(PhysFrame::containing_address(p4e.addr()));
        let p3e = &mut pml3[i3];
        if p3e.flags().contains(PageTableFlags::PRESENT) {
            let new_flags = p3e.flags() | flags;
            if new_flags != p3e.flags() {
                p3e.set_flags(new_flags);
            }
            let pml2 = pt_from_frame(PhysFrame::containing_address(p3e.addr()));
            let p2e = &mut pml2[i2];
            if p2e.flags().contains(PageTableFlags::PRESENT)
                && !p2e.flags().contains(PageTableFlags::HUGE_PAGE)
            {
                let new_flags = p2e.flags() | flags;
                if new_flags != p2e.flags() {
                    p2e.set_flags(new_flags);
                }
            }
        }
    }
}

/// Assert that every present intermediate paging-structure entry leading to
/// `virt_addr` has `WRITABLE | USER_ACCESSIBLE`. Effective x86-64 access is
/// the AND across all levels, so a restrictive intermediate silently makes
/// every leaf in the sub-range read-only or kernel-only. Debug-only.
///
/// Only checked for user-half addresses; kernel intermediates may legitimately
/// lack USER_ACCESSIBLE.
#[cfg(debug_assertions)]
pub fn debug_assert_permissive_parents(pml4_frame: PhysFrame, virt_addr: VirtAddr) {
    if virt_addr.as_u64() >= 0x0000_8000_0000_0000 {
        return;
    }
    let a = virt_addr.as_u64();
    let i4 = ((a >> 39) & 0x1FF) as usize;
    let i3 = ((a >> 30) & 0x1FF) as usize;
    let i2 = ((a >> 21) & 0x1FF) as usize;
    let phys_off = boot_info().physical_memory_offset;
    let want = PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    let pt_from_frame = |f: PhysFrame| -> &'static PageTable {
        let virt = phys_off + f.start_address().as_u64();
        // SAFETY: f names a page table frame reached by walking the live hierarchy,
        // mapped through the physical offset. Shared read-only.
        unsafe { &*virt.as_ptr::<PageTable>() }
    };
    let p4 = pt_from_frame(pml4_frame);
    let p4e = &p4[i4];
    if !p4e.flags().contains(PageTableFlags::PRESENT) {
        return;
    }
    debug_assert!(
        p4e.flags().contains(want),
        "pml4[{i4}] missing {want:?} for user va {virt_addr:?} (flags={:?})",
        p4e.flags()
    );
    let p3 = pt_from_frame(PhysFrame::containing_address(p4e.addr()));
    let p3e = &p3[i3];
    if !p3e.flags().contains(PageTableFlags::PRESENT) {
        return;
    }
    debug_assert!(
        p3e.flags().contains(want),
        "pml3[{i3}] missing {want:?} for user va {virt_addr:?} (flags={:?})",
        p3e.flags()
    );
    let p2 = pt_from_frame(PhysFrame::containing_address(p3e.addr()));
    let p2e = &p2[i2];
    if !p2e.flags().contains(PageTableFlags::PRESENT)
        || p2e.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return;
    }
    debug_assert!(
        p2e.flags().contains(want),
        "pml2[{i2}] missing {want:?} for user va {virt_addr:?} (flags={:?})",
        p2e.flags()
    );
}
