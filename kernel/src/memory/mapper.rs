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
    memory::{STACK_ALIGNMENT, frame_allocator::frame_allocator},
    thread::irqlock::IrqLockGuard,
};

/// OS-available PTE bit used to mark copy-on-write pages.
pub const COW_BIT: PageTableFlags = PageTableFlags::BIT_9;

pub unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_table_frame, _) = Cr3::read();

    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    unsafe { &mut *page_table_ptr }
}

pub unsafe fn get_level_4_table(cr3: (PhysFrame, Cr3Flags)) -> &'static mut PageTable {
    let physical_memory_offset = boot_info().physical_memory_offset;

    let phys = cr3.0.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    unsafe { &mut *page_table_ptr }
}

pub fn memory_mapper() -> IrqLockGuard<'static, MemoryManager> {
    boot_info().memory_manager.lock()
}

#[derive(Debug)]
pub struct MemoryManager {
    pub mapper: OffsetPageTable<'static>,
}

#[expect(unused)]
impl MemoryManager {
    pub fn new(page_table: OffsetPageTable<'static>) -> Self {
        Self { mapper: page_table }
    }

    /// Maps memory, the default flag is PRESENT, use extra flags for more.
    pub fn map_memory(
        &mut self,
        addr: VirtAddr,
        size: u64,
        extra_flags: PageTableFlags,
    ) -> Result<PageRangeInclusive<Size4KiB>, MapToError<Size4KiB>> {
        let page_range = get_page_range(addr, size);

        let flags = PageTableFlags::PRESENT | extra_flags;
        {
            let mut frame_allocator = frame_allocator();

            for page in page_range {
                let frame = frame_allocator
                    .allocate_frame()
                    .ok_or(MapToError::FrameAllocationFailed)?;
                unsafe {
                    self.mapper
                        .map_to(page, frame, flags, &mut *frame_allocator)?
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

        let flags = PageTableFlags::PRESENT | extra_flags;
        {
            let mut frame_allocator = frame_allocator();
            let frame = frame_allocator
                .allocate_contiguous_frames(page_range.count())
                .ok_or(MapToError::FrameAllocationFailed)?;

            for (i, page) in page_range.enumerate() {
                let current_frame = PhysFrame::containing_address(
                    frame.start_address() + (i as u64 * Size4KiB::SIZE),
                );
                unsafe {
                    self.mapper
                        .map_to(page, current_frame, flags, &mut *frame_allocator)?
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
    pub fn map_address(
        &mut self,
        virt_addr: VirtAddr,
        phys_addr: PhysAddr,
        flags: PageTableFlags,
    ) -> Result<(), x86_64::structures::paging::mapper::MapToError<Size4KiB>> {
        let page = Page::containing_address(virt_addr);
        let frame = PhysFrame::containing_address(phys_addr);
        let mut frame_allocator = frame_allocator();

        unsafe {
            self.mapper
                .map_to(
                    page,
                    frame,
                    PageTableFlags::PRESENT | flags,
                    &mut *frame_allocator,
                )?
                .flush()
        };

        if let Some(idx) = frame_allocator.frame_to_index(frame) {
            frame_allocator.set_frame_allocated(idx);
        }

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

    /// Translate a virtual address mapped in this page table to a kernel HHDM pointer.
    fn translate_to_hhdm_ptr(&self, vaddr: VirtAddr) -> Option<*mut u8> {
        match self.mapper.translate(vaddr) {
            TranslateResult::Mapped { frame, offset, .. } => {
                let phys = frame.start_address() + offset;
                let hhdm = crate::boot::boot_info().physical_memory_offset;
                Some((hhdm + phys.as_u64()).as_mut_ptr())
            }
            _ => None,
        }
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
            unsafe {
                core::ptr::write_bytes(dest_ptr, 0, chunk);
            }
            offset += chunk;
        }
    }

    /// Write a value to user virtual address space via HHDM.
    /// Uses copy_to_user internally so it handles page boundaries safely.
    pub fn write_val_to_user<T: Copy>(&self, dest_vaddr: VirtAddr, value: T) {
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

        unsafe {
            self.mapper
                .clean_up_addr_range(lower_half, &mut *frame_allocator())
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
