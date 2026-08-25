use alloc::sync::Arc;
use alloc::vec::Vec;

/// A single `R_X86_64_RELATIVE` entry, compressed to 12 bytes.
///
/// Does not implement `Debug` directly; see `RelocTable` for the top-level type.
///
/// `offset` is the page-relative u32 (max 4 GiB load image; all current
/// EDOS binaries are well under this). `addend` is the full i64 r_addend
/// from the ELF relocation record.
#[derive(Clone, Debug)]
pub struct RelocEntry {
    /// Byte offset from the ELF load base; u32 caps the load image at 4 GiB.
    pub offset: u32,
    /// Addend for the RELATIVE relocation: value = base + addend.
    pub addend: i64,
}

/// Per-page bucket index into `entries`.
///
/// For a fault on page at load-image offset `page_off`, the bucket is
/// `buckets[page_off >> 12]`. Entries for that page are
/// `entries[bucket.start .. bucket.start + bucket.len]`.
#[derive(Clone, Debug)]
struct Bucket {
    /// Index into `RelocTable::entries` of the first entry for this page.
    start: u32,
    /// Number of entries for this page.
    len: u32,
}

/// Table of all `R_X86_64_RELATIVE` relocations for a loaded ELF binary,
/// indexed by page for O(1) lookup in the fault handler.
#[derive(Clone, Debug)]
pub struct RelocTable {
    /// All entries, sorted by `offset` (ascending).
    entries: Vec<RelocEntry>,
    /// One bucket per 4-KiB page of the writable PT_LOAD VMA.
    /// `buckets[page_off >> 12]` gives the slice of `entries` for that page.
    buckets: Vec<Bucket>,
    /// Page-aligned start of the writable PT_LOAD VMA (load-image relative).
    vma_start_page_offset: u32,
}

impl RelocTable {
    /// Build a `RelocTable` from raw `(r_offset, r_addend)` pairs.
    ///
    /// `load_base`: virtual base at which the ELF is loaded (used only for
    /// the straddling-reloc assert message; not stored).
    ///
    /// `vma_page_start`: page-aligned start of the writable PT_LOAD VMA,
    /// as a load-image-relative byte offset (i.e. already subtracted from
    /// the virtual address so it starts at a page boundary ≤ the first
    /// reloc target in that VMA).
    ///
    /// `vma_page_count`: number of 4-KiB pages in the writable PT_LOAD VMA.
    ///
    /// Panics if any reloc straddles a 4-KiB page boundary (reviewer
    /// mandate: assert `(r_offset & 0xfff) <= 0xff8`).
    pub fn build(
        mut raw: Vec<(u32, i64)>,
        vma_page_start: u32,
        vma_page_count: usize,
    ) -> Arc<Self> {
        // Sort by offset so bucket ranges are contiguous.
        raw.sort_unstable_by_key(|&(off, _)| off);

        // Validate: no straddling relocs.
        for &(off, _) in &raw {
            assert!(
                (off & 0xfff) <= 0xff8,
                "lazy reloc: R_X86_64_RELATIVE at offset {:#x} straddles a \
                 page boundary (low 12 bits = {:#x}); zero straddlers expected",
                off,
                off & 0xfff
            );
        }

        let entries: Vec<RelocEntry> = raw
            .iter()
            .map(|&(offset, addend)| RelocEntry { offset, addend })
            .collect();

        // Build per-page buckets. Each bucket covers one 4-KiB page of the
        // writable PT_LOAD VMA. Entries outside [vma_page_start, vma_page_start
        // + vma_page_count * 4096) are ignored; none are expected, but a
        // stray one must not index outside the bucket table.
        let mut buckets: Vec<Bucket> = Vec::with_capacity(vma_page_count);
        for _ in 0..vma_page_count {
            buckets.push(Bucket { start: 0, len: 0 });
        }

        for (entry_idx, entry) in entries.iter().enumerate() {
            let off = entry.offset;
            // Skip entries outside this VMA.
            if off < vma_page_start {
                continue;
            }
            let vma_off = off - vma_page_start;
            let page_idx = (vma_off >> 12) as usize;
            if page_idx >= vma_page_count {
                continue;
            }
            let bucket = &mut buckets[page_idx];
            if bucket.len == 0 {
                bucket.start = entry_idx as u32;
            }
            bucket.len += 1;
        }

        Arc::new(Self {
            entries,
            buckets,
            vma_start_page_offset: vma_page_start,
        })
    }

    /// Number of entries in the relocation table.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Number of page buckets that contain at least one relocation entry.
    pub fn populated_buckets(&self) -> usize {
        self.buckets.iter().filter(|b| b.len > 0).count()
    }

    /// Return the slice of `RelocEntry` whose targets land in the page
    /// at `page_vaddr_offset` (page-aligned byte offset from the ELF load
    /// image base).
    pub fn entries_for_page(&self, page_vaddr_offset: u32) -> &[RelocEntry] {
        if page_vaddr_offset < self.vma_start_page_offset {
            return &[];
        }
        let vma_off = page_vaddr_offset - self.vma_start_page_offset;
        let page_idx = (vma_off >> 12) as usize;
        if page_idx >= self.buckets.len() {
            return &[];
        }
        let bucket = &self.buckets[page_idx];
        if bucket.len == 0 {
            return &[];
        }
        let start = bucket.start as usize;
        let end = start + bucket.len as usize;
        &self.entries[start..end]
    }

    /// Apply all relocations that land in the page whose first byte is at
    /// `frame_virt` (a kernel HHDM pointer to a freshly allocated private
    /// frame, NOT the cache frame).
    ///
    /// `page_vaddr_offset`: page-aligned byte offset of this page from the ELF
    /// load base (i.e. `page_vaddr - load_base`, already page-aligned).
    ///
    /// `base`: ELF load base as a raw u64 (`load_base.as_u64()`).
    ///
    /// # Safety
    /// `frame_virt` must point to 4096 writable bytes accessible from the
    /// kernel (HHDM-mapped private frame). The caller is responsible for
    /// ensuring no concurrent access to this frame.
    pub unsafe fn apply_relocs_to_page(
        &self,
        frame_virt: x86_64::VirtAddr,
        page_vaddr_offset: u32,
        base: u64,
    ) {
        let entries = self.entries_for_page(page_vaddr_offset);
        for entry in entries {
            // Within-page byte offset of this relocation target.
            let page_byte_off = (entry.offset & 0xfff) as usize;
            let ptr = (frame_virt.as_u64() + page_byte_off as u64) as *mut u64;
            unsafe {
                *ptr = base.wrapping_add(entry.addend as u64);
            }
        }
    }
}
