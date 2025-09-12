use core::{marker::PhantomData, ptr};

use alloc::{
    collections::{btree_map::BTreeMap, vec_deque::VecDeque},
    vec::Vec,
};
use x86_64::{
    PhysAddr, VirtAddr, align_up, instructions::interrupts::without_interrupts,
    registers::control::Cr3, structures::paging::PageTableFlags,
};

use crate::{
    boot::boot_info,
    drivers::ahci::AhciError,
    memory::{DMA_REGION_START, mapper::memory_mapper},
};

#[derive(Debug, Clone, Copy)]
pub struct DmaRegion<T: 'static> {
    pub virt_addr: VirtAddr,
    _phantom: PhantomData<T>,
}

// TODO: Add a dma pool instead of this hack.
static NEXT_DMA_ADDR: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(DMA_REGION_START.as_u64());

impl<T> DmaRegion<T> {
    pub fn get(&self) -> *mut T {
        self.virt_addr.as_mut_ptr()
    }

    pub fn allocate() -> Result<Self, AhciError> {
        let size = core::mem::size_of::<T>() as u64 * 2;
        let aligned_size = (size + 0xfff) & !0xfff; // Round up to page boundary

        let virt_addr = VirtAddr::new(
            NEXT_DMA_ADDR.fetch_add(aligned_size, core::sync::atomic::Ordering::Relaxed),
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
                    .map_err(|_| AhciError::DmaAllocationFailed)
            })?;
        }

        // Zero the memory
        unsafe {
            ptr::write_bytes(virt_addr.as_mut_ptr::<T>(), 0, 1);
        }

        Ok(Self {
            virt_addr,
            _phantom: PhantomData,
        })
    }

    pub fn phys_addr(&self) -> PhysAddr {
        let mapper = memory_mapper();
        match mapper.translate(self.virt_addr) {
            x86_64::structures::paging::mapper::TranslateResult::Mapped {
                frame, offset, ..
            } => frame.start_address() + offset,
            _ => panic!("DMA region not mapped!"),
        }
    }
}

/// Variable-sized DMA buffer for multi-sector operations
#[derive(Debug)]
pub struct DmaBuffer {
    pub virt_addr: VirtAddr,
    pub size: usize,
}

impl DmaBuffer {
    pub fn allocate_sized(size: usize) -> Result<Self, AhciError> {
        let aligned_size = (size as u64 + 0xfff) & !0xfff; // Round up to page boundary

        let virt_addr = VirtAddr::new(
            NEXT_DMA_ADDR.fetch_add(aligned_size, core::sync::atomic::Ordering::Relaxed),
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
                    .map_err(|_| AhciError::DmaAllocationFailed)
            })?;
        }

        // Zero the memory
        unsafe {
            ptr::write_bytes(virt_addr.as_mut_ptr::<u8>(), 0, size);
        }

        Ok(Self { virt_addr, size })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.virt_addr.as_mut_ptr()
    }

    pub fn phys_addr(&self) -> PhysAddr {
        let mapper = memory_mapper();
        match mapper.translate(self.virt_addr) {
            x86_64::structures::paging::mapper::TranslateResult::Mapped {
                frame, offset, ..
            } => frame.start_address() + offset,
            _ => panic!("DMA buffer not mapped!"),
        }
    }
}

#[derive(Debug, Default)]
pub struct DmaAllocator {
    list_512: Vec<DmaBuffer>,
    list_1024: Vec<DmaBuffer>,
    list_2048: Vec<DmaBuffer>,
    list_big: BTreeMap<usize, Vec<DmaBuffer>>,
}

impl DmaAllocator {
    pub fn allocate_sized(&mut self, size: usize) -> Result<DmaBuffer, AhciError> {
        if size <= 512 {
            if let Some(buf) = self.list_512.pop() {
                Ok(buf)
            } else {
                DmaBuffer::allocate_sized(512)
            }
        } else if size <= 1024 {
            if let Some(buf) = self.list_1024.pop() {
                Ok(buf)
            } else {
                DmaBuffer::allocate_sized(1024)
            }
        } else if size <= 2048 {
            if let Some(buf) = self.list_2048.pop() {
                Ok(buf)
            } else {
                DmaBuffer::allocate_sized(2048)
            }
        } else {
            for (found_size, buffers) in self.list_big.iter_mut() {
                if *found_size >= size
                    && let Some(buf) = buffers.pop()
                {
                    return Ok(buf);
                }
            }

            DmaBuffer::allocate_sized(align_up(size as u64, 4096) as usize)
        }
    }

    pub fn dealloc(&mut self, buffer: DmaBuffer) {
        match buffer.size {
            512 => {
                self.list_512.push(buffer);
            }
            1024 => {
                self.list_1024.push(buffer);
            }
            2048 => {
                self.list_2048.push(buffer);
            }
            _ => {
                let entry = self.list_big.entry(buffer.size).or_default();
                entry.push(buffer);
            }
        }
    }
}
