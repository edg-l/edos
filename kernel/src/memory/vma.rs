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
    Physical { phys_base: u64 },
    /// Shared memory region
    SharedMemory { shm_id: u64 },
    /// TLS region
    Tls,
    /// Stack
    Stack,
    /// File-backed mapping (MAP_PRIVATE or MAP_SHARED).
    /// `pages` has one slot per VMA page (length = VMA size / 4096), initially all None.
    /// Demand faults fill the slots; msync and munmap read them back.
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

/// Exclusive upper bound of the user half of the canonical address space.
pub const USER_VA_END: u64 = 0x0000_8000_0000_0000;

/// Mapping granularity: the smallest region the MMU can give one mapping.
pub const PAGE_SIZE: u64 = 4096;

/// `addr` rounded up to the next page boundary, or `None` if that wraps.
pub fn page_round_up(addr: u64) -> Option<u64> {
    Some(addr.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmaError {
    /// The range wraps, or reaches past [`USER_VA_END`] into the kernel half.
    OutOfUserSpace,
    /// The user half holds no free gap of the requested length.
    NoSpace,
}

/// Ordered collection of VMAs keyed by start address
#[derive(Debug)]
pub struct VmaSet {
    vmas: BTreeMap<VirtAddr, Vma>,
    /// Lowest address `find_free_address` may hand out, learned from the first
    /// request. Zero until then. Wrapping the cursor back to this floor is what
    /// lets freed ranges be reused instead of leaked.
    mmap_base: AtomicU64,
}

impl Clone for VmaSet {
    fn clone(&self) -> Self {
        Self {
            vmas: self.vmas.clone(),
            mmap_base: AtomicU64::new(self.mmap_base.load(Ordering::Relaxed)),
        }
    }
}

impl VmaSet {
    pub fn new() -> Self {
        Self {
            vmas: BTreeMap::new(),
            mmap_base: AtomicU64::new(0),
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

    /// Insert a VMA, rejecting any range that is not wholly within the user half.
    ///
    /// Every range in a `VmaSet` becomes a `USER_ACCESSIBLE` mapping, and two of
    /// the addresses that reach here are untrusted: an explicit `mmap` address
    /// and an ELF `p_vaddr`. The bound is checked here rather than at those call
    /// sites so that a future caller cannot forget it; the address-picking path
    /// ([`Self::find_free_address`]) enforces the same bound on its own results.
    pub fn insert(&mut self, vma: Vma) -> Result<(), VmaError> {
        if vma.end < vma.start || vma.end.as_u64() > USER_VA_END {
            return Err(VmaError::OutOfUserSpace);
        }
        self.vmas.insert(vma.start, vma);
        Ok(())
    }

    /// Insert a range whose bound was already established: one handed back by an
    /// unmap failure path, one copied from a parent's set on fork, or one the
    /// kernel derived from `USER_STACK_TOP`. These callers have no error to
    /// report and no untrusted input, so the bound is a debug assertion.
    pub fn insert_validated(&mut self, vma: Vma) {
        debug_assert!(
            vma.end >= vma.start && vma.end.as_u64() <= USER_VA_END,
            "insert_validated called with a range outside the user half"
        );
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

    /// Claims a free range of `length` bytes for a new VMA and returns its start.
    ///
    /// Finding the range and inserting the VMA happen under the one acquisition
    /// the caller already holds. Splitting them across two acquisitions is a race
    /// with no narrow window to argue about: two threads both run the search,
    /// both see the same gap, and both map there, so their pages alias and each
    /// scribbles over the other's. Building `backing` before the lock keeps the
    /// allocation for a file-backed page vector outside the critical section.
    pub fn reserve(
        &mut self,
        hint: &AtomicU64,
        length: u64,
        prot: VmaProt,
        flags: VmaFlags,
        backing: VmaBacking,
    ) -> Result<VirtAddr, VmaError> {
        // The MMU maps whole pages, so a mapping owns every page it touches. A
        // VMA recording the unrounded length would end mid-page and let the next
        // mapping start in that same page, where a zero-fill fault or an unmap
        // on either one destroys the other's data.
        let length = page_round_up(length).ok_or(VmaError::OutOfUserSpace)?;

        let start = self.find_free_address(hint, length)?;
        let end = start
            .as_u64()
            .checked_add(length)
            .ok_or(VmaError::OutOfUserSpace)?;
        debug_assert!(
            start.as_u64().is_multiple_of(PAGE_SIZE) && end.is_multiple_of(PAGE_SIZE),
            "reserve produced a range that does not cover whole pages"
        );
        self.insert(Vma {
            start,
            end: VirtAddr::new(end),
            prot,
            flags,
            backing,
        })?;
        Ok(start)
    }

    /// Find a free virtual address range of `length` bytes.
    ///
    /// `hint` is a cursor, not an allocator: the search starts there to keep
    /// consecutive mappings cheap to find, but when the space above it is
    /// exhausted the search wraps to the base and reuses holes left by
    /// `munmap`. Private because a returned address is only free while the lock
    /// is held; callers claim a range through [`Self::reserve`].
    fn find_free_address(&self, hint: &AtomicU64, length: u64) -> Result<VirtAddr, VmaError> {
        // A length within a page of u64::MAX would wrap this to a small value and
        // hand out a gap far shorter than the caller asked for.
        let len = page_round_up(length).ok_or(VmaError::OutOfUserSpace)?;

        // The first caller defines the floor: below it lie the image, heap and
        // anything else the loader placed, none of which this allocator owns.
        let mut base = self.mmap_base.load(Ordering::Relaxed);
        if base == 0 {
            // Align up: every result derives from this floor or from a VMA end,
            // so a misaligned base would misalign every mapping after it.
            base = (hint.load(Ordering::Relaxed) + 0xfff) & !0xfff;
            self.mmap_base.store(base, Ordering::Relaxed);
        }

        // Always search from the base, never from the previous allocation.
        // Starting at a cursor and only falling back to the base on exhaustion
        // looks like an optimisation, but with 128 TiB above the cursor the
        // fallback never runs, so freed ranges are never reused and the cursor
        // marches upward exactly as the old bump allocator did. The scan is
        // over a process's VMA list, which is tens of entries.
        let found = self.first_fit(base, len).ok_or(VmaError::NoSpace)?;

        hint.store(found + len, Ordering::Relaxed);
        Ok(VirtAddr::new(found))
    }

    /// Lowest gap of `len` bytes at or above `floor`, or `None` if the address
    /// space above `floor` cannot hold one.
    ///
    /// The cursor is page-aligned past every VMA end, so a gap never begins
    /// inside a page another mapping already owns. Not every VMA covers whole
    /// pages: an ELF segment's bound comes from `p_memsz`.
    fn first_fit(&self, floor: u64, len: u64) -> Option<u64> {
        let mut cursor = floor;

        // A VMA starting below the floor can still extend past it.
        if let Some((_, prev)) = self.vmas.range(..=VirtAddr::new(floor)).next_back() {
            cursor = cursor.max(page_round_up(prev.end.as_u64())?);
        }

        for (start, vma) in self.vmas.range(VirtAddr::new(floor)..) {
            if start.as_u64() >= cursor.checked_add(len)? {
                return Some(cursor);
            }
            cursor = cursor.max(page_round_up(vma.end.as_u64())?);
        }

        (cursor.checked_add(len)? <= USER_VA_END).then_some(cursor)
    }
}
