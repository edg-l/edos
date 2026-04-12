use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::VirtAddr;

use bitflags::bitflags;

use crate::fs::inode::VfsInode;
use crate::fs::page_cache::CachedPage;

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

#[derive(Clone)]
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
    /// ELF segment backed by in-kernel ELF data.
    /// Pages are faulted in on demand from the stored ELF data.
    ElfSegment {
        /// The full ELF file data (shared across all segments of the same binary)
        elf_data: Arc<Vec<u8>>,
        /// Offset of this segment's data within the ELF file
        file_offset: u64,
        /// Size of file-backed data (bytes beyond this up to VMA size are BSS/zero)
        file_size: u64,
        /// Offset of the segment's virtual address within the page-aligned VMA start
        /// (i.e., vaddr - page_aligned_vaddr). Needed because the VMA start is page-aligned
        /// but the segment data may start at an offset within that first page.
        vaddr_offset: u64,
    },
    /// TLS region
    Tls,
    /// Stack
    Stack,
    /// File-backed mapping (MAP_PRIVATE or MAP_SHARED).
    /// `pages` has one slot per VMA page (length = VMA size / 4096), initially all None.
    /// Phase B fills slots during demand faults; Phase C uses them for msync/munmap.
    FileBacked {
        inode: Arc<VfsInode>,
        /// Byte offset in the file corresponding to the VMA start (4 KiB aligned).
        file_offset: u64,
        /// true = MAP_SHARED, false = MAP_PRIVATE.
        shared: bool,
        /// true if PROT_WRITE was requested at mmap time.
        writable_mapping: bool,
        /// Per-page Arc to the cached page, indexed by (virt_page - vma_start) / 4096.
        /// None = not yet faulted in (or COW'd away to a private frame).
        pages: Vec<Option<Arc<CachedPage>>>,
    },
}

impl core::fmt::Debug for VmaBacking {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Anonymous => write!(f, "Anonymous"),
            Self::Physical { phys_base } => f
                .debug_struct("Physical")
                .field("phys_base", phys_base)
                .finish(),
            Self::SharedMemory { shm_id } => f
                .debug_struct("SharedMemory")
                .field("shm_id", shm_id)
                .finish(),
            Self::ElfSegment {
                file_offset,
                file_size,
                vaddr_offset,
                ..
            } => f
                .debug_struct("ElfSegment")
                .field("file_offset", file_offset)
                .field("file_size", file_size)
                .field("vaddr_offset", vaddr_offset)
                .finish(),
            Self::Tls => write!(f, "Tls"),
            Self::Stack => write!(f, "Stack"),
            Self::FileBacked {
                inode,
                file_offset,
                shared,
                writable_mapping,
                pages,
            } => f
                .debug_struct("FileBacked")
                .field("mount_id", &inode.mount_id)
                .field("ino", &inode.ino)
                .field("file_offset", file_offset)
                .field("shared", shared)
                .field("writable_mapping", writable_mapping)
                .field("pages_len", &pages.len())
                .finish(),
        }
    }
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

    /// Find the VMA containing the given address (mutable).
    pub fn find_mut(&mut self, addr: VirtAddr) -> Option<&mut Vma> {
        let key = self
            .vmas
            .range(..=addr)
            .next_back()
            .map(|(k, _)| *k)
            .filter(|&k| self.vmas[&k].contains(addr))?;
        self.vmas.get_mut(&key)
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

    /// Split the VMA containing `addr` at a page-aligned boundary so that `addr`
    /// becomes the start of the second half.  Does nothing if `addr` is already
    /// the start of a VMA or if no VMA contains `addr`.
    ///
    /// For `FileBacked` VMAs the per-page Arc vec is split at the corresponding
    /// index and the second half's `file_offset` is adjusted accordingly.
    pub fn split_at(&mut self, addr: VirtAddr) {
        let aligned = addr.align_down(4096u64);

        // Find the VMA that contains `aligned` strictly inside (not at its start).
        let start_key = match self
            .vmas
            .range(..aligned)
            .next_back()
            .map(|(k, _)| *k)
            .filter(|&k| {
                let vma = &self.vmas[&k];
                vma.contains(aligned) && vma.start != aligned
            }) {
            Some(k) => k,
            None => return,
        };

        let mut original = self.vmas.remove(&start_key).unwrap();
        let split_byte_offset = aligned.as_u64() - original.start.as_u64();
        let split_page_idx = (split_byte_offset / 4096) as usize;

        // Build the tail VMA.
        let tail_backing = match &mut original.backing {
            VmaBacking::FileBacked {
                inode,
                file_offset,
                shared,
                writable_mapping,
                pages,
            } => {
                let tail_pages = pages.split_off(split_page_idx);
                let tail_file_offset = *file_offset + split_byte_offset;
                VmaBacking::FileBacked {
                    inode: Arc::clone(inode),
                    file_offset: tail_file_offset,
                    shared: *shared,
                    writable_mapping: *writable_mapping,
                    pages: tail_pages,
                }
            }
            other => other.clone(),
        };

        let tail = Vma {
            start: aligned,
            end: original.end,
            prot: original.prot,
            flags: original.flags,
            backing: tail_backing,
        };
        original.end = aligned;

        self.vmas.insert(original.start, original);
        self.vmas.insert(tail.start, tail);
    }

    /// Remove all VMAs fully contained in `[start, end)`, splitting VMAs that
    /// straddle the boundaries as needed.  Returns the removed VMAs.
    pub fn remove_range(&mut self, start: VirtAddr, end: VirtAddr) -> Vec<Vma> {
        self.split_at(start);
        self.split_at(end);

        let keys: Vec<VirtAddr> = self.vmas.range(start..end).map(|(k, _)| *k).collect();

        let mut removed = Vec::new();
        for k in keys {
            if let Some(vma) = self.vmas.remove(&k) {
                removed.push(vma);
            }
        }
        removed
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
