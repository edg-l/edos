use core::{marker::PhantomData, ptr};

use crossbeam_queue::SegQueue;
use spin::Once;
use thiserror::Error;
use x86_64::{
    PhysAddr, VirtAddr, align_up, instructions::interrupts::without_interrupts,
    structures::paging::PageTableFlags,
};

use crate::memory::{
    mapper::memory_mapper,
    valloc::{vfree, vmalloc},
};

static DMA_ALLOCATOR: DmaAllocator = DmaAllocator::new();

pub fn dma() -> &'static DmaAllocator {
    &DMA_ALLOCATOR
}

#[derive(Debug)]
pub struct DmaRegion<T: 'static> {
    pub buffer: DmaBuffer,
    _phantom: PhantomData<T>,
}

impl<T> DmaRegion<T> {
    pub fn get(&self) -> *mut T {
        self.buffer.as_ptr().cast()
    }

    /// # Safety
    /// Caller must ensure buffer has enough size for T.
    unsafe fn from_buffer(buffer: DmaBuffer) -> Self {
        Self {
            buffer,
            _phantom: PhantomData,
        }
    }

    #[expect(unused)]
    pub fn into_buffer(self) -> DmaBuffer {
        self.buffer
    }

    pub fn phys_addr(&self) -> PhysAddr {
        self.buffer.phys_addr()
    }
}

/// Variable-sized DMA buffer for multi-sector operations
#[derive(Debug)]
pub struct DmaBuffer {
    pub virt_addr: VirtAddr,
    pub size: usize,
    phys_addr: Once<PhysAddr>,
    /// Set on a buffer backed by newly mapped frames, cleared once it has been
    /// recycled through a bucket. Only a fresh buffer needs zeroing: a recycled
    /// one is already covered by the documented no-zeroing contract.
    fresh: bool,
}

impl DmaBuffer {
    /// Allocate a DMA buffer of the given size (rounded up to page boundary).
    /// Private: all external callers must use `dma().allocate_sized()`.
    fn allocate_sized(size: usize) -> Result<Self, DmaError> {
        let aligned_size = (size as u64 + 0xfff) & !0xfff; // Round up to page boundary

        let virt_addr = vmalloc(aligned_size).ok_or(DmaError::DmaAllocationFailed)?;

        {
            without_interrupts(|| {
                let mut mapper = memory_mapper();

                mapper
                    .map_memory_contiguous(
                        virt_addr,
                        aligned_size,
                        PageTableFlags::WRITABLE
                            | PageTableFlags::NO_CACHE
                            | PageTableFlags::GLOBAL,
                    )
                    .map_err(|_| DmaError::DmaAllocationFailed)
            })?;
        }

        Ok(Self {
            virt_addr,
            size: aligned_size as usize,
            phys_addr: Once::new(),
            fresh: true,
        })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.virt_addr.as_mut_ptr()
    }

    pub fn phys_addr(&self) -> PhysAddr {
        *self.phys_addr.call_once(|| {
            let mapper = memory_mapper();
            match mapper.translate(self.virt_addr) {
                x86_64::structures::paging::mapper::TranslateResult::Mapped {
                    frame,
                    offset,
                    ..
                } => frame.start_address() + offset,
                _ => panic!("DMA buffer not mapped!"),
            }
        })
    }

    fn dealloc(&self) -> Result<(), DmaError> {
        without_interrupts(|| -> Result<(), DmaError> {
            let mut mapper = memory_mapper();

            mapper
                .unmap_memory(self.virt_addr, self.size as u64)
                .map_err(|_| DmaError::DmaAllocationFailed)?;

            vfree(self.virt_addr, self.size as u64);

            Ok(())
        })
    }
}

/// Power-of-two DMA buffer pool.
///
/// Buckets cover 4KB (2^12) through 2MB (2^21) = 10 size classes.
/// Requests are rounded up to the next power-of-two page size and pooled
/// at that tier, giving automatic "close enough" size matching.
/// Sizes above 2MB are allocated directly and freed on dealloc (no pooling).
pub struct DmaAllocator {
    buckets: [SegQueue<DmaBuffer>; NUM_BUCKETS],
}

const NUM_BUCKETS: usize = 10; // 4KB..2MB
const MIN_ORDER: usize = 12; // log2(4096)

/// Map a requested size to a bucket index, or None if > 2MB.
fn size_to_bucket(size: usize) -> Option<usize> {
    let size = size.max(4096).next_power_of_two();
    let order = size.trailing_zeros() as usize;
    let idx = order.checked_sub(MIN_ORDER)?;
    if idx < NUM_BUCKETS { Some(idx) } else { None }
}

/// The allocation size for a given bucket index.
fn bucket_size(bucket: usize) -> usize {
    1 << (bucket + MIN_ORDER)
}

impl DmaAllocator {
    pub const fn new() -> Self {
        Self {
            buckets: [const { SegQueue::new() }; NUM_BUCKETS],
        }
    }

    pub fn allocate<T: Sized>(&self) -> Result<DmaRegion<T>, DmaError> {
        let buffer = self.allocate_sized(core::mem::size_of::<T>())?;

        Ok(unsafe { DmaRegion::from_buffer(buffer) })
    }

    /// Allocate a DMA buffer of at least `size` bytes, zeroed only when its
    /// frames are newly mapped.
    ///
    /// A buffer served from a bucket still holds whatever its previous owner
    /// left there, so a caller that needs the whole region zeroed must zero it
    /// itself; only a device transfer's own byte count may be read back. A
    /// *fresh* buffer is zeroed because a structure the device reads before the
    /// driver has written every field needs defined contents to begin with ---
    /// an e1000e TX ring's status bytes decide which buffers `tx_clean`
    /// reclaims, and nothing initialises them.
    ///
    /// Use [`Self::allocate_sized_uninit`] where a copy overwrites the whole
    /// buffer before the device sees it: the mapping is uncached, so zeroing a
    /// page costs about 58 us.
    pub fn allocate_sized(&self, size: usize) -> Result<DmaBuffer, DmaError> {
        let buf = self.allocate_sized_uninit(size)?;
        if buf.fresh {
            // SAFETY: `buf` owns `buf.size` bytes of mapped, writable memory.
            unsafe {
                ptr::write_bytes(buf.as_ptr(), 0, buf.size);
            }
        }
        Ok(buf)
    }

    /// Allocate a DMA buffer of at least `size` bytes, zeroing nothing.
    ///
    /// The buffer holds whatever its frames last did, so every byte the device
    /// or the caller will read must be written first. This exists because the
    /// DMA mapping is uncached: zeroing costs ~58 us per 4 KiB page, which is
    /// worth avoiding for a bounce buffer that a copy immediately overwrites.
    pub fn allocate_sized_uninit(&self, size: usize) -> Result<DmaBuffer, DmaError> {
        if let Some(bucket) = size_to_bucket(size) {
            let alloc_size = bucket_size(bucket);
            if let Some(buf) = self.buckets[bucket].pop() {
                Ok(buf)
            } else {
                DmaBuffer::allocate_sized(alloc_size)
            }
        } else {
            // > 2MB: direct allocation, no pooling
            let alloc_size = align_up(size as u64, 4096) as usize;
            DmaBuffer::allocate_sized(alloc_size)
        }
    }

    pub fn dealloc(&self, mut buffer: DmaBuffer) -> Result<(), DmaError> {
        buffer.fresh = false;
        if let Some(bucket) = size_to_bucket(buffer.size) {
            self.buckets[bucket].push(buffer);
            Ok(())
        } else {
            buffer.dealloc()
        }
    }
}

#[derive(Debug, Error, Clone, Copy)]
pub enum DmaError {
    #[error("dma allocation failed")]
    DmaAllocationFailed,
}
