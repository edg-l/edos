use spin::MutexGuard;
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags,
        PhysFrame, Size4KiB,
        mapper::{FlagUpdateError, MapToError, UnmapError},
        page::PageRangeInclusive,
    },
};

use crate::{
    boot::boot_info,
    memory::{STACK_ALIGNMENT, frame_allocator::frame_allocator},
};

pub unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_table_frame, _) = Cr3::read();

    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    unsafe { &mut *page_table_ptr }
}

pub fn memory_mapper() -> MutexGuard<'static, MemoryManager> {
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
}

pub fn get_page_range(addr: VirtAddr, size: u64) -> PageRangeInclusive<Size4KiB> {
    let start_page = Page::containing_address(addr);

    // Calculate the number of pages needed to cover the entire size.
    let page_count = size.div_ceil(Size4KiB::SIZE);

    // Calculate the ending page by adding the page count to the starting page.
    // This avoids creating a non-canonical intermediate virtual address.
    let end_page = start_page + (page_count - 1);

    // Create an inclusive range of pages.
    Page::range_inclusive(start_page, end_page)
}

/// Align a stack pointer down to the required stack alignment (16 bytes for FPU/SSE)
pub fn align_stack_pointer(stack_ptr: VirtAddr) -> VirtAddr {
    VirtAddr::new(stack_ptr.as_u64() & !(STACK_ALIGNMENT - 1))
}
