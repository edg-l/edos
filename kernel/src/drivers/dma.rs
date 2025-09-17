use core::{marker::PhantomData, ptr};

use alloc::{collections::btree_map::BTreeMap, vec::Vec};
use crossbeam_queue::SegQueue;
use spin::Once;
use thiserror::Error;
use x86_64::{
    PhysAddr, VirtAddr, align_up, instructions::interrupts::without_interrupts,
    structures::paging::PageTableFlags,
};

use crate::{
    log,
    memory::{DMA_REGION_START, mapper::memory_mapper},
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

// TODO: Add a dma pool instead of this hack.
static NEXT_DMA_ADDR: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(DMA_REGION_START.as_u64());

impl<T> DmaRegion<T> {
    pub fn get(&self) -> *mut T {
        self.buffer.as_ptr().cast()
    }

    fn allocate() -> Result<Self, DmaError> {
        let buffer = DmaBuffer::allocate_sized(core::mem::size_of::<T>())?;

        Ok(Self {
            buffer,
            _phantom: PhantomData,
        })
    }

    /// # Safety
    /// Caller must ensure buffer has enough size for T.
    unsafe fn from_buffer(buffer: DmaBuffer) -> Self {
        Self {
            buffer,
            _phantom: PhantomData,
        }
    }

    fn into_buffer(self) -> DmaBuffer {
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
}

impl DmaBuffer {
    pub fn allocate_sized(size: usize) -> Result<Self, DmaError> {
        let aligned_size = (size as u64 + 0xfff) & !0xfff; // Round up to page boundary

        let virt_addr = VirtAddr::new(
            NEXT_DMA_ADDR.fetch_add(aligned_size, core::sync::atomic::Ordering::Relaxed),
        );

        log!(
            "Allocating dma buffer at: {virt_addr:?} {:?}",
            virt_addr + aligned_size
        );

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

        // Zero the memory
        unsafe {
            ptr::write_bytes(virt_addr.as_mut_ptr::<u8>(), 0, aligned_size as usize);
        }

        Ok(Self {
            virt_addr,
            size: aligned_size as usize,
            phys_addr: Once::new(),
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
        without_interrupts(|| {
            let mut mapper = memory_mapper();

            mapper
                .unmap_memory(self.virt_addr, self.size as u64)
                .map_err(|_| DmaError::DmaAllocationFailed)
        })?;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct DmaAllocator {
    list_4096: SegQueue<DmaBuffer>,
    list_8192: SegQueue<DmaBuffer>,
    list_16384: SegQueue<DmaBuffer>,
}

impl DmaAllocator {
    pub const fn new() -> Self {
        Self {
            list_4096: SegQueue::new(),
            list_8192: SegQueue::new(),
            list_16384: SegQueue::new(),
        }
    }

    pub fn allocate<T: Sized>(&self) -> Result<DmaRegion<T>, DmaError> {
        let buffer = self.allocate_sized(core::mem::size_of::<T>())?;

        Ok(unsafe { DmaRegion::from_buffer(buffer) })
    }

    pub fn allocate_sized(&self, size: usize) -> Result<DmaBuffer, DmaError> {
        // Since dma are page aligned we use page sizes.
        if size <= 4096 {
            if let Some(buf) = self.list_4096.pop() {
                Ok(buf)
            } else {
                log!("Allocating new 4kb dma buffer");
                DmaBuffer::allocate_sized(4096)
            }
        } else if size <= 8192 {
            if let Some(buf) = self.list_8192.pop() {
                Ok(buf)
            } else {
                log!("Allocating new 8kb dma buffer");
                DmaBuffer::allocate_sized(8192)
            }
        } else if size <= 16384 {
            if let Some(buf) = self.list_16384.pop() {
                Ok(buf)
            } else {
                log!("Allocating new 16kb dma buffer");
                DmaBuffer::allocate_sized(16384)
            }
        } else {
            log!("Allocating new big {size} dma buffer");
            DmaBuffer::allocate_sized(align_up(size as u64, 4096) as usize)
        }
    }

    pub fn dealloc(&self, buffer: DmaBuffer) -> Result<(), DmaError> {
        match buffer.size {
            4096 => {
                self.list_4096.push(buffer);
            }
            8192 => {
                self.list_8192.push(buffer);
            }
            16384 => {
                self.list_16384.push(buffer);
            }
            _ => {
                buffer.dealloc()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, Copy)]
pub enum DmaError {
    #[error("dma allocation failed")]
    DmaAllocationFailed,
}
