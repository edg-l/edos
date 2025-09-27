//! A bitmap based frame allocator
//!
//! The allocator finds first a memory region to allocate itself into, the bitmap.

use limine::{memory_map::EntryType, response::MemoryMapResponse};
use spin::Once;
use x86_64::{
    PhysAddr,
    structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame, Size4KiB},
};

use crate::{
    memory::get_virt_addr_from_phys_offset,
    thread::irqlock::{IrqLockGuard, IrqSpinlock},
};

static FRAME_ALLOCATOR: Once<IrqSpinlock<BitmapFrameAllocator>> = Once::new();

#[must_use]
pub fn frame_allocator() -> IrqLockGuard<'static, BitmapFrameAllocator> {
    FRAME_ALLOCATOR.get().unwrap().lock()
}

/// Initialize the frame allocator using bootloader memory for bitmap storage
pub fn init_frame_allocator(memory_regions: &'static MemoryMapResponse) {
    // Calculate required bitmap size
    let required_size = calculate_bitmap_size(memory_regions);

    // Find a suitable region for the bitmap
    let (bitmap_storage, storage_start_addr, storage_size) =
        find_bitmap_storage(memory_regions, required_size)
            .expect("No suitable memory region found for frame bitmap");

    // Create and initialize the allocator
    let mut allocator = BitmapFrameAllocator::new(memory_regions, bitmap_storage);

    // Mark bitmap storage frames as allocated
    let frame_count = storage_size.div_ceil(4096); // Round up to frames
    for i in 0..frame_count {
        let frame_addr = storage_start_addr + (i * 4096) as u64;
        let frame = PhysFrame::containing_address(PhysAddr::new(frame_addr));

        if let Some(index) = allocator.frame_to_index(frame) {
            allocator.set_frame_allocated(index);
        }
    }

    FRAME_ALLOCATOR.call_once(|| IrqSpinlock::new(allocator));
}

/// Find suitable memory for bitmap storage
fn find_bitmap_storage(
    memory_regions: &MemoryMapResponse,
    required_size: usize,
) -> Option<(&'static mut [u8], u64, usize)> {
    let usable_regions = memory_regions
        .entries()
        .iter()
        .filter(|r| r.entry_type == EntryType::USABLE);
    let mut addr_ranges = usable_regions.map(|r| r.base..(r.base + r.length));

    let mut current = addr_ranges.next()?;

    // Check if first range is sufficient
    if current.end - current.start >= required_size as u64 {
        let phys_addr = PhysAddr::new(current.start);
        let virt_addr = get_virt_addr_from_phys_offset(phys_addr);

        unsafe {
            let ptr = virt_addr.as_mut_ptr::<u8>();
            let storage = core::slice::from_raw_parts_mut(ptr, required_size);
            return Some((storage, current.start, required_size));
        }
    }

    for range in addr_ranges {
        if current.end == range.start {
            // Extend current range
            current.end = range.end;

            // Check if it fits now
            if current.end - current.start >= required_size as u64 {
                let phys_addr = PhysAddr::new(current.start);
                let virt_addr = get_virt_addr_from_phys_offset(phys_addr);

                unsafe {
                    let ptr = virt_addr.as_mut_ptr::<u8>();
                    let storage = core::slice::from_raw_parts_mut(ptr, required_size);
                    return Some((storage, current.start, required_size));
                }
            }
        } else {
            // Gap found, start new range
            current = range;

            // Check if this new range is sufficient
            if current.end - current.start >= required_size as u64 {
                let phys_addr = PhysAddr::new(current.start);
                let virt_addr = get_virt_addr_from_phys_offset(phys_addr);

                unsafe {
                    let ptr = virt_addr.as_mut_ptr::<u8>();
                    let storage = core::slice::from_raw_parts_mut(ptr, required_size);
                    return Some((storage, current.start, required_size));
                }
            }
        }
    }

    None
}

/// Bitmap-based frame allocator for efficient frame management
pub struct BitmapFrameAllocator {
    /// Bitmap where each bit represents a frame (0 = free, 1 = allocated)
    bitmap: &'static mut [u8],
    /// Physical address of the first frame managed by this allocator
    start_frame: PhysFrame,
    /// Total number of frames managed
    frame_count: usize,
    /// Hint for the next potentially free frame index (optimization)
    next_free_hint: usize,
}

#[expect(unused)]
impl BitmapFrameAllocator {
    /// Create a new bitmap frame allocator
    ///
    /// # Arguments
    ///
    /// * `memory_regions` - Available memory regions from bootloader
    /// * `bitmap_storage` - Pre-allocated storage for the bitmap
    ///
    /// # Returns
    ///
    /// Returns a new bitmap frame allocator managing all usable frames
    pub fn new(
        memory_regions: &'static MemoryMapResponse,
        bitmap_storage: &'static mut [u8],
    ) -> Self {
        let (start_frame, frame_count) = calculate_frame_range(memory_regions);

        // Calculate required bitmap size (1 bit per frame)
        let required_bytes = frame_count.div_ceil(8);
        assert!(
            bitmap_storage.len() >= required_bytes,
            "Bitmap storage too small: need {} bytes, got {}",
            required_bytes,
            bitmap_storage.len()
        );

        let mut allocator = Self {
            bitmap: bitmap_storage,
            start_frame,
            frame_count,
            next_free_hint: 0,
        };

        // Mark non-usable frames as allocated
        allocator.mark_non_usable_frames(memory_regions);

        allocator
    }

    /// Mark non-usable frames as allocated in the bitmap
    fn mark_non_usable_frames(&mut self, memory_regions: &MemoryMapResponse) {
        // First mark all frames as allocated
        for byte in self.bitmap.iter_mut() {
            *byte = 0xFF;
        }

        // Then mark usable regions as free
        for region in memory_regions.entries() {
            if region.entry_type == EntryType::USABLE {
                let start_frame = PhysFrame::containing_address(PhysAddr::new(region.base));
                let end_frame =
                    PhysFrame::containing_address(PhysAddr::new(region.base + region.length - 1));

                if let Some(start_idx) = self.frame_to_index(start_frame)
                    && let Some(end_idx) = self.frame_to_index(end_frame)
                {
                    for frame_idx in start_idx..=end_idx {
                        self.set_frame_free(frame_idx);
                    }
                }
            }
        }
    }

    /// Convert frame to bitmap index
    pub fn frame_to_index(&self, frame: PhysFrame) -> Option<usize> {
        let frame_addr = frame.start_address().as_u64();
        let start_addr = self.start_frame.start_address().as_u64();

        if frame_addr < start_addr {
            return None;
        }

        let index = ((frame_addr - start_addr) / 4096) as usize;
        if index >= self.frame_count {
            None
        } else {
            Some(index)
        }
    }

    /// Convert bitmap index to frame
    fn index_to_frame(&self, index: usize) -> Option<PhysFrame> {
        if index >= self.frame_count {
            return None;
        }

        let frame_addr = self.start_frame.start_address().as_u64() + (index as u64 * 4096);
        Some(PhysFrame::containing_address(PhysAddr::new(frame_addr)))
    }

    /// Check if a frame is allocated
    fn is_frame_allocated(&self, index: usize) -> bool {
        if index >= self.frame_count {
            return true; // Out of bounds frames are considered allocated
        }

        let byte_index = index / 8;
        let bit_index = index % 8;
        (self.bitmap[byte_index] & (1 << bit_index)) != 0
    }

    /// Mark a frame as allocated
    pub(super) fn set_frame_allocated(&mut self, index: usize) {
        if index >= self.frame_count {
            return;
        }

        let byte_index = index / 8;
        let bit_index = index % 8;
        self.bitmap[byte_index] |= 1 << bit_index;
    }

    /// Mark a frame as free
    #[inline]
    fn set_frame_free(&mut self, index: usize) {
        if index >= self.frame_count {
            return;
        }

        let byte_index = index / 8;
        let bit_index = index % 8;
        self.bitmap[byte_index] &= !(1 << bit_index);

        // Update hint if this is before our current hint
        if index < self.next_free_hint {
            self.next_free_hint = index;
        }
    }

    /// Find the next free frame starting from the hint
    fn find_free_frame(&mut self) -> Option<usize> {
        // Start from hint and wrap around
        for i in 0..self.frame_count {
            let index = (self.next_free_hint + i) % self.frame_count;
            if !self.is_frame_allocated(index) {
                self.next_free_hint = index + 1;
                return Some(index);
            }
        }
        None
    }

    /// Allocate contiguous frames for DMA operations
    ///
    /// # Arguments
    ///
    /// * `count` - Number of contiguous frames needed
    ///
    /// # Returns
    ///
    /// Returns the first frame of the contiguous block, or None if not available
    pub fn allocate_contiguous_frames(&mut self, count: usize) -> Option<PhysFrame> {
        if count == 0 {
            return None;
        }

        // For single frame, use regular allocation
        if count == 1 {
            return self.allocate_frame();
        }

        // Search for contiguous free frames
        for start_idx in 0..=(self.frame_count.saturating_sub(count)) {
            let mut all_free = true;

            // Check if all frames in range are free
            for offset in 0..count {
                if self.is_frame_allocated(start_idx + offset) {
                    all_free = false;
                    break;
                }
            }

            if all_free {
                // Mark all frames as allocated
                for offset in 0..count {
                    self.set_frame_allocated(start_idx + offset);
                }

                // Update hint
                self.next_free_hint = start_idx + count;

                return self.index_to_frame(start_idx);
            }
        }

        None
    }

    /// Deallocate contiguous frames
    ///
    /// # Arguments
    ///
    /// * `start_frame` - First frame of the contiguous block
    /// * `count` - Number of frames to deallocate
    ///
    /// # Safety
    ///
    /// The frames must have been allocated by this allocator and not be in use
    pub unsafe fn deallocate_contiguous_frames(&mut self, start_frame: PhysFrame, count: usize) {
        if let Some(start_idx) = self.frame_to_index(start_frame) {
            for offset in 0..count {
                if start_idx + offset < self.frame_count {
                    self.set_frame_free(start_idx + offset);
                }
            }
        }
    }

    /// Deallocate a single frame
    ///
    /// # Safety
    ///
    /// The frame must not be in use and must have been allocated by this allocator
    pub unsafe fn deallocate_frame(&mut self, frame: PhysFrame) {
        if let Some(index) = self.frame_to_index(frame) {
            self.set_frame_free(index);
        }
    }

    /// Get allocator statistics
    pub fn stats(&self) -> FrameAllocatorStats {
        let mut allocated_frames = 0;

        for byte in self.bitmap.iter() {
            allocated_frames += byte.count_ones() as usize;
        }

        FrameAllocatorStats {
            total_frames: self.frame_count,
            allocated_frames,
            free_frames: self.frame_count - allocated_frames,
        }
    }
}

/// Calculate the range of frames managed by this allocator
fn calculate_frame_range(memory_regions: &MemoryMapResponse) -> (PhysFrame, usize) {
    let usable_regions = memory_regions
        .entries()
        .iter()
        .filter(|r| r.entry_type == EntryType::USABLE);

    let min_addr = usable_regions
        .clone()
        .map(|r| r.base)
        .min()
        .expect("No usable memory regions found");
    let max_addr = usable_regions.map(|r| r.base + r.length).max().unwrap();

    let start_frame = PhysFrame::containing_address(PhysAddr::new(min_addr));
    let end_frame: PhysFrame<Size4KiB> = PhysFrame::containing_address(PhysAddr::new(max_addr - 1));
    let frame_count =
        (end_frame.start_address().as_u64() - start_frame.start_address().as_u64()) / 4096 + 1;

    (start_frame, frame_count as usize)
}

pub fn calculate_bitmap_size(memory_regions: &MemoryMapResponse) -> usize {
    let (_, frame_count) = calculate_frame_range(memory_regions);
    frame_count.div_ceil(8) // Round up to nearest byte
}

unsafe impl FrameAllocator<Size4KiB> for BitmapFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        if let Some(index) = self.find_free_frame() {
            self.set_frame_allocated(index);
            self.index_to_frame(index)
        } else {
            None
        }
    }
}

impl FrameDeallocator<Size4KiB> for BitmapFrameAllocator {
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame<Size4KiB>) {
        unsafe {
            self.deallocate_frame(frame);
        }
    }
}

#[expect(unused)]
#[derive(Debug, Clone, Copy)]
pub struct FrameAllocatorStats {
    pub total_frames: usize,
    pub allocated_frames: usize,
    pub free_frames: usize,
}
