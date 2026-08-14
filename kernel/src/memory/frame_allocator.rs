//! A bitmap based frame allocator
//!
//! The allocator finds first a memory region to allocate itself into, the bitmap.

use limine::{memmap, request::MemmapResponse};
use spin::Once;
use x86_64::{
    PhysAddr,
    structures::paging::{FrameAllocator, FrameDeallocator, PhysFrame, Size4KiB},
};

use crate::{
    debug::lock_order::{RANK_FRAME_ALLOC, RankedGuard},
    memory::get_virt_addr_from_phys_offset,
    thread::irqlock::{IrqLockGuard, IrqSpinlock},
};

static FRAME_ALLOCATOR: Once<IrqSpinlock<BitmapFrameAllocator>> = Once::new();

#[must_use]
pub fn frame_allocator() -> RankedGuard<IrqLockGuard<'static, BitmapFrameAllocator>> {
    FRAME_ALLOCATOR
        .get()
        .unwrap()
        .lock_ranked(RANK_FRAME_ALLOC, "frame_alloc")
}

/// Initialize the frame allocator using bootloader memory for bitmap storage
pub fn init_frame_allocator(memory_regions: &'static MemmapResponse) {
    let (_, frame_count) = calculate_frame_range(memory_regions);
    // Round bitmap_bytes up to 2-byte alignment so the refcount array (u16) is properly aligned
    let bitmap_bytes = frame_count.div_ceil(8).next_multiple_of(2);
    let refcount_bytes = frame_count * 2; // u16 per frame
    let total_size = bitmap_bytes + refcount_bytes;

    // Find a suitable region for both the bitmap and refcount array
    let (combined_storage, storage_start_addr, storage_size) =
        find_bitmap_storage(memory_regions, total_size)
            .expect("No suitable memory region found for frame bitmap");

    // Split storage: first bitmap_bytes for the bitmap, rest for refcounts
    let (bitmap_storage, refcount_raw) = combined_storage.split_at_mut(bitmap_bytes);

    // Reinterpret refcount_raw as &'static mut [u16]
    let refcount_storage: &'static mut [u16] = unsafe {
        core::slice::from_raw_parts_mut(refcount_raw.as_mut_ptr() as *mut u16, frame_count)
    };

    // Zero-initialize refcounts
    for rc in refcount_storage.iter_mut() {
        *rc = 0;
    }

    // Create and initialize the allocator
    let mut allocator = BitmapFrameAllocator::new(memory_regions, bitmap_storage, refcount_storage);

    // Mark bitmap storage frames as allocated
    let frame_count_storage = storage_size.div_ceil(4096); // Round up to frames
    for i in 0..frame_count_storage {
        let frame_addr = storage_start_addr + (i * 4096) as u64;
        let frame = PhysFrame::containing_address(PhysAddr::new(frame_addr));

        if let Some(index) = allocator.frame_to_index(frame) {
            allocator.set_frame_allocated(index);
        }
    }

    FRAME_ALLOCATOR.call_once(|| IrqSpinlock::new(allocator));
}

/// True when `frame` can never be handed out: either it is already allocated,
/// or it falls outside the range the bitmap covers.
///
/// Used to assert that memory the kernel borrows from the bootloader (Limine
/// modules, which live in bootloader-reclaimable regions) is not also in the
/// allocator's free pool.
pub fn is_frame_reserved(frame: PhysFrame) -> bool {
    let alloc = frame_allocator();
    alloc
        .frame_to_index(frame)
        .is_none_or(|index| alloc.is_frame_allocated(index))
}

/// Find suitable memory for bitmap storage
fn find_bitmap_storage(
    memory_regions: &MemmapResponse,
    required_size: usize,
) -> Option<(&'static mut [u8], u64, usize)> {
    let usable_regions = memory_regions
        .entries()
        .iter()
        .filter(|r| r.type_ == memmap::MEMMAP_USABLE);
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
    /// Reference counts per frame (one u16 per physical frame)
    refcounts: &'static mut [u16],
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
        memory_regions: &'static MemmapResponse,
        bitmap_storage: &'static mut [u8],
        refcount_storage: &'static mut [u16],
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
        assert!(
            refcount_storage.len() >= frame_count,
            "Refcount storage too small: need {} entries, got {}",
            frame_count,
            refcount_storage.len()
        );

        let mut allocator = Self {
            bitmap: bitmap_storage,
            refcounts: refcount_storage,
            start_frame,
            frame_count,
            next_free_hint: 0,
        };

        // Mark non-usable frames as allocated
        allocator.mark_non_usable_frames(memory_regions);

        allocator
    }

    /// Mark non-usable frames as allocated in the bitmap
    fn mark_non_usable_frames(&mut self, memory_regions: &MemmapResponse) {
        // First mark all frames as allocated
        for byte in self.bitmap.iter_mut() {
            *byte = 0xFF;
        }

        // Then mark usable regions as free
        for region in memory_regions.entries() {
            if region.type_ == memmap::MEMMAP_USABLE {
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

    /// Mark a frame as allocated without giving it an owner: the bitmap bit
    /// stops the frame being handed out, and the refcount stays at 0 so
    /// `deallocate_frame` refuses to free it. This is the state for memory the
    /// allocator does not own — firmware tables, MMIO, boot-time reservations.
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

    /// Find first free frame index at or after `start` using byte-level scan.
    /// Returns None if no free frame exists from `start` to end of bitmap.
    fn scan_free_from(&self, start: usize) -> Option<usize> {
        if start >= self.frame_count {
            return None;
        }
        let total_bytes = self.frame_count.div_ceil(8);
        let byte_idx = start / 8;
        let bit_offset = start % 8;

        // Check partial first byte (mask lower bits as "allocated")
        if byte_idx < total_bytes {
            let mask = (1u8 << bit_offset) - 1;
            let byte = self.bitmap[byte_idx] | mask;
            if byte != 0xFF {
                let index = byte_idx * 8 + (!byte).trailing_zeros() as usize;
                if index < self.frame_count {
                    return Some(index);
                }
            }
        }

        // Scan full bytes (8 frames per iteration)
        for bi in (byte_idx + 1)..total_bytes {
            if self.bitmap[bi] != 0xFF {
                let index = bi * 8 + (!self.bitmap[bi]).trailing_zeros() as usize;
                if index < self.frame_count {
                    return Some(index);
                }
            }
        }

        None
    }

    /// Find the next free frame starting from the hint, with byte-level scan.
    fn find_free_frame(&mut self) -> Option<usize> {
        if let Some(idx) = self.scan_free_from(self.next_free_hint) {
            self.next_free_hint = idx + 1;
            return Some(idx);
        }
        // Wrap around: scan from beginning up to where we started
        if self.next_free_hint > 0
            && let Some(idx) = self.scan_free_from(0)
        {
            self.next_free_hint = idx + 1;
            return Some(idx);
        }
        None
    }

    /// Allocate contiguous frames for DMA operations.
    ///
    /// Uses byte-level scanning and skip-ahead: when an allocated frame is
    /// found mid-run, jumps past it instead of retrying from the next index.
    pub fn allocate_contiguous_frames(&mut self, count: usize) -> Option<PhysFrame> {
        if count == 0 {
            return None;
        }
        if count == 1 {
            return self.allocate_frame();
        }

        let max_start = self.frame_count.saturating_sub(count);
        let mut idx = 0;

        while idx <= max_start {
            // Byte-level fast skip: all-allocated bytes jump 8 frames
            let byte_idx = idx / 8;
            if idx % 8 == 0 && self.bitmap[byte_idx] == 0xFF {
                idx += 8;
                continue;
            }

            if self.is_frame_allocated(idx) {
                idx += 1;
                continue;
            }

            // Found a free frame at idx; verify `count` contiguous free frames
            let mut run = 1;
            while run < count {
                let pos = idx + run;
                // Byte-level fast check: 8 free frames at once when aligned
                if pos % 8 == 0 && run + 8 <= count && self.bitmap[pos / 8] == 0x00 {
                    run += 8;
                    continue;
                }
                if self.is_frame_allocated(pos) {
                    // Skip-ahead: jump past the allocated frame
                    idx = pos + 1;
                    break;
                }
                run += 1;
            }

            if run >= count {
                for offset in 0..count {
                    debug_assert!(
                        self.refcounts[idx + offset] == 0,
                        "allocate_contiguous_frames: frame index {} has refcount {} but bitmap says free (double-alloc)",
                        idx + offset,
                        self.refcounts[idx + offset]
                    );
                    self.set_frame_allocated(idx + offset);
                    self.refcounts[idx + offset] = 1;
                }
                self.next_free_hint = idx + count;
                return self.index_to_frame(idx);
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
                let idx = start_idx + offset;
                if idx < self.frame_count {
                    if self.refcounts[idx] > 1 {
                        self.refcounts[idx] -= 1;
                    } else {
                        self.refcounts[idx] = 0;
                        self.set_frame_free(idx);
                    }
                }
            }
        }
    }

    /// Deallocate a single frame
    ///
    /// # Safety
    ///
    /// The frame must not be in use and must have been allocated by this allocator.
    ///
    /// Frames mapped via `map_address` / `map_address_range` (MMIO, pre-existing
    /// physical ranges) have `refcount == 0` — calling `deallocate_frame` on
    /// those is a bug that would leak the bitmap bit. We panic instead of
    /// silently clearing state.
    pub unsafe fn deallocate_frame(&mut self, frame: PhysFrame) {
        if let Some(index) = self.frame_to_index(frame) {
            assert!(
                self.refcounts[index] > 0,
                "deallocate_frame: frame idx {} (phys {:#x}) has refcount=0; caller is freeing a non-allocator-owned frame (MMIO?) or double-freeing",
                index,
                frame.start_address().as_u64()
            );
            if self.refcounts[index] > 1 {
                self.refcounts[index] -= 1;
                return;
            }
            self.refcounts[index] = 0;
            self.set_frame_free(index);
        }
    }

    /// Returns the reference count for a frame, or 0 if out of range.
    pub fn refcount(&self, frame: PhysFrame) -> u16 {
        match self.frame_to_index(frame) {
            Some(idx) => self.refcounts[idx],
            None => 0,
        }
    }

    /// Increment the reference count for a frame. Panics on u16 overflow.
    pub fn inc_refcount(&mut self, frame: PhysFrame) {
        if let Some(idx) = self.frame_to_index(frame) {
            self.refcounts[idx] = self.refcounts[idx]
                .checked_add(1)
                .expect("inc_refcount: refcount overflow");
        }
    }

    /// Decrement the reference count for a frame and return the new value.
    /// Panics if the refcount is already 0.
    pub fn dec_refcount(&mut self, frame: PhysFrame) -> u16 {
        if let Some(idx) = self.frame_to_index(frame) {
            assert!(
                self.refcounts[idx] > 0,
                "dec_refcount: refcount is already 0 for frame {:?}",
                frame
            );
            self.refcounts[idx] -= 1;
            self.refcounts[idx]
        } else {
            0
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
fn calculate_frame_range(memory_regions: &MemmapResponse) -> (PhysFrame, usize) {
    let usable_regions = memory_regions
        .entries()
        .iter()
        .filter(|r| r.type_ == memmap::MEMMAP_USABLE);

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

#[expect(unused)]
pub fn calculate_bitmap_size(memory_regions: &MemmapResponse) -> usize {
    let (_, frame_count) = calculate_frame_range(memory_regions);
    // Round bitmap_bytes up to 2-byte alignment so the refcount array (u16) is properly aligned
    let bitmap_bytes = frame_count.div_ceil(8).next_multiple_of(2);
    let refcount_bytes = frame_count * 2; // u16 per frame
    bitmap_bytes + refcount_bytes
}

unsafe impl FrameAllocator<Size4KiB> for BitmapFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        if let Some(index) = self.find_free_frame() {
            // Bitmap and refcount must agree: a frame found via find_free_frame
            // (which scans the bitmap) MUST have refcount==0. If refcount>0 the
            // bitmap and refcount are inconsistent — we are about to hand out a
            // frame that is still in use somewhere.
            debug_assert!(
                self.refcounts[index] == 0,
                "allocate_frame: frame index {} has refcount {} but bitmap says free (double-alloc)",
                index,
                self.refcounts[index]
            );
            self.set_frame_allocated(index);
            self.refcounts[index] = 1;
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
