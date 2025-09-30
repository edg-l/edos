use core::{marker::PhantomData, ptr};

use crossbeam_queue::SegQueue;
use spin::Once;
use thiserror::Error;
use x86_64::{
    PhysAddr, VirtAddr, align_up, instructions::interrupts::without_interrupts,
    structures::paging::PageTableFlags,
};

use crate::{
    log,
    memory::{
        mapper::memory_mapper,
        valloc::{vfree, vmalloc},
    },
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
}

impl DmaBuffer {
    pub fn allocate_sized(size: usize) -> Result<Self, DmaError> {
        let aligned_size = (size as u64 + 0xfff) & !0xfff; // Round up to page boundary

        let virt_addr = vmalloc(aligned_size);

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
        without_interrupts(|| -> Result<(), DmaError> {
            let mut mapper = memory_mapper();

            mapper
                .unmap_memory(self.virt_addr, self.size as u64)
                .map_err(|_| DmaError::DmaAllocationFailed)?;

            vfree(self.virt_addr);

            Ok(())
        })
    }
}

#[derive(Debug, Default)]
pub struct DmaAllocator {
    list_4096: SegQueue<DmaBuffer>,
    list_8192: SegQueue<DmaBuffer>,
    list_16384: SegQueue<DmaBuffer>,
    // ahci may use up to 2mb
    list_2mb: SegQueue<DmaBuffer>,
}

impl DmaAllocator {
    pub const fn new() -> Self {
        Self {
            list_4096: SegQueue::new(),
            list_8192: SegQueue::new(),
            list_16384: SegQueue::new(),
            list_2mb: SegQueue::new(),
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
        } else if size == 2097152 {
            // 2mb case, max dma region for our AHCI driver.
            if let Some(buf) = self.list_2mb.pop() {
                Ok(buf)
            } else {
                log!("Allocating new 2mb dma buffer");
                DmaBuffer::allocate_sized(2097152)
            }
        } else {
            let size = align_up(size as u64, 4096);
            log!("Allocating new big {size} dma buffer");
            DmaBuffer::allocate_sized(size as usize)
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
            2097152 => {
                self.list_2mb.push(buffer);
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
