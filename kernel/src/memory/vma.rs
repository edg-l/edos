use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::VirtAddr;

use bitflags::bitflags;

bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct VmaProt: u8 {
        const READ  = 0x1;
        const WRITE = 0x2;
        const EXEC  = 0x4;
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct VmaFlags: u8 {
        const PRIVATE    = 0x01;
        const SHARED     = 0x02;
        const GROWSDOWN  = 0x04;  // stack
        const LAZY       = 0x08;  // pages not pre-faulted (future use)
    }
}

#[derive(Debug, Clone)]
pub enum VmaBacking {
    /// Zero-fill on demand
    Anonymous,
    /// Physical/MMIO mapping (frames not owned by allocator)
    Physical {
        #[allow(dead_code)]
        phys_base: u64,
    },
    /// Shared memory region
    SharedMemory { shm_id: u64 },
    /// ELF segment (eagerly loaded)
    Elf,
    /// TLS region
    Tls,
    /// Stack
    Stack,
}

#[derive(Debug, Clone)]
pub struct Vma {
    pub start: VirtAddr,
    /// End address (exclusive)
    pub end: VirtAddr,
    pub prot: VmaProt,
    pub flags: VmaFlags,
    pub backing: VmaBacking,
}

impl Vma {
    pub fn contains(&self, addr: VirtAddr) -> bool {
        addr >= self.start && addr < self.end
    }

    pub fn size(&self) -> u64 {
        self.end.as_u64() - self.start.as_u64()
    }
}

/// Ordered collection of VMAs keyed by start address
#[derive(Debug, Clone)]
pub struct VmaSet {
    vmas: BTreeMap<VirtAddr, Vma>,
}

impl VmaSet {
    pub fn new() -> Self {
        Self {
            vmas: BTreeMap::new(),
        }
    }

    /// Find the VMA containing the given address
    pub fn find(&self, addr: VirtAddr) -> Option<&Vma> {
        self.vmas
            .range(..=addr)
            .next_back()
            .map(|(_, vma)| vma)
            .filter(|vma| vma.contains(addr))
    }

    pub fn insert(&mut self, vma: Vma) {
        self.vmas.insert(vma.start, vma);
    }

    pub fn remove(&mut self, start: &VirtAddr) -> Option<Vma> {
        self.vmas.remove(start)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Vma> {
        self.vmas.values()
    }

    pub fn len(&self) -> usize {
        self.vmas.len()
    }

    /// Find free virtual address for a mapping of the given length,
    /// starting from `hint` and advancing atomically.
    pub fn find_free_address(&self, hint: &AtomicU64, length: u64) -> VirtAddr {
        let aligned_length = (length + 0xfff) & !0xfff;

        loop {
            let candidate_u64 = hint.fetch_add(aligned_length, Ordering::Relaxed);
            let candidate = VirtAddr::new(candidate_u64);
            let end_addr = candidate + aligned_length;

            let mut overlaps = false;
            for vma in self.vmas.values() {
                let vma_end = vma.end;
                if !(end_addr <= vma.start || candidate >= vma_end) {
                    overlaps = true;
                    break;
                }
                if vma.start > end_addr {
                    break;
                }
            }

            if !overlaps {
                return candidate;
            }
        }
    }
}
