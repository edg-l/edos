pub mod reloc;

use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use thiserror::Error;
use x86_64::{VirtAddr, align_up, structures::paging::PageTableFlags};

use reloc::RelocTable;

use crate::{
    fs::{
        api as fs_api,
        inode::VfsInode,
        page_fill,
        path::Path,
        vfs::{fs_by_mount_id, get_or_fill_page},
    },
    log_debug,
    memory::{
        frame_allocator::frame_allocator,
        mapper::MemoryManager,
        vma::{USER_VA_END, Vma, VmaBacking, VmaFlags, VmaProt},
    },
    println,
};

/// Read `len` bytes from a file at `offset` via the VFS page cache.
/// Used to read ELF headers, relocation sections, and TLS init data
/// without materialising the entire binary in kernel memory.
fn read_file_range(path: &Path, offset: u64, len: u64) -> Result<Vec<u8>, ElfLoadError> {
    fs_api::read_bytes(path, offset as usize, len as usize).map_err(|_| ElfLoadError::MappingFailed)
}

#[derive(Debug, Clone)]
pub struct TlsTemplate {
    pub init_data: Vec<u8>,
    pub mem_size: u64,
    pub align: u64,
}

#[derive(Debug, Clone)]
pub struct LoadedInfo {
    pub entry_point: VirtAddr,
    pub heap_break: u64,
    pub memory_regions: Vec<Vma>,
    pub tls_template: Option<TlsTemplate>,
    /// Parsed `R_X86_64_RELATIVE` relocation table for lazy page-fault application.
    /// `None` if the binary has no relocations.
    pub reloc_table: Option<Arc<RelocTable>>,
    /// Virtual address range of the writable PT_LOAD VMA that contains reloc targets.
    pub reloc_vma_range: Option<core::ops::Range<VirtAddr>>,
    /// ELF load base (0 for ET_EXEC, 0x400000 for ET_DYN static-PIE).
    pub load_base: u64,
}

#[derive(Debug, Error)]
pub enum ElfLoadError {
    #[error("UnsupportedArchitecture")]
    UnsupportedArchitecture,
    #[error("MappingFailed")]
    MappingFailed,
    #[error("MissingSegments")]
    MissingSegments,
    #[error("NoEntryPoint")]
    NoEntryPoint,
    /// The inode's filesystem does not support the page cache.
    /// Callers map this to ENOEXEC so non-page-cache binaries fail at spawn.
    #[error("NoPageCache")]
    NoPageCache,
    /// A PT_LOAD segment does not lie wholly within the user half, or its
    /// header fields do not describe a coherent range.
    #[error("InvalidSegment")]
    InvalidSegment,
    /// The file does not start with the ELF identification bytes.
    #[error("NotAnElf")]
    NotAnElf,
    /// A relocation the loader understands, but whose fields are not coherent:
    /// an `R_X86_64_RELATIVE` naming a symbol, a target past the 4 GiB the
    /// reloc table addresses, or targets that no single writable `PT_LOAD`
    /// contains.
    #[error("InvalidRelocation")]
    InvalidRelocation,
    /// A relocation kind EDOS does not implement. Binaries are static-PIE, so
    /// `SHT_REL` and the dynamic-symbol kinds never appear in a valid one.
    #[error("UnsupportedRelocation")]
    UnsupportedRelocation,
}

/// Read a little-endian integer out of an ELF structure.
///
/// Every field this loader parses is attacker-controlled: any user can spawn
/// any file it can read, so a truncated or crafted header must produce an
/// error rather than an out-of-bounds slice. Each returns `None` when the
/// field does not lie wholly within `bytes`.
fn le_u16(bytes: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(off..off + 2)?.try_into().ok()?,
    ))
}

fn le_u32(bytes: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(off..off + 4)?.try_into().ok()?,
    ))
}

fn le_u64(bytes: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(off..off + 8)?.try_into().ok()?,
    ))
}

fn le_i64(bytes: &[u8], off: usize) -> Option<i64> {
    Some(i64::from_le_bytes(
        bytes.get(off..off + 8)?.try_into().ok()?,
    ))
}

/// Allocate a private frame, copy one page from the inode page cache into it,
/// and map it at `page_addr` with the given flags.
///
/// `zero_from` optionally zeroes bytes `[zero_from..4096]` after the memcpy.
/// Used for the last file-data page of a PT_LOAD whose `p_filesz` is not a
/// multiple of 4096: bytes past `p_filesz` are the start of BSS and must read
/// as zero per the ELF spec (the "tail zero" requirement), but the cache page
/// contains whatever the linker left on disk (usually alignment junk). This
/// pre-fault is the only caller; the lazy reloc path cannot fire on an
/// already-mapped page, so relocs in this page are applied immediately after
/// the page is mapped (see the reloc_pages walk below).
///
/// The cache page is pinned only long enough for the memcpy; it is unpinned
/// before returning so the cache can evict it independently. The private frame
/// is fully independent of the cache.
fn prefault_elf_tail_page_from_cache(
    memory_manager: &mut MemoryManager,
    inode: &Arc<VfsInode>,
    fs: &Arc<dyn crate::fs::FileSystem + Send + Sync>,
    file_page_idx: u64,
    page_addr: VirtAddr,
    flags: PageTableFlags,
    zero_from: Option<usize>,
) -> Result<(), ElfLoadError> {
    use x86_64::structures::paging::FrameAllocator;

    // Allocate a private frame and zero it (not a cache frame).
    let private_frame = frame_allocator()
        .allocate_frame()
        .ok_or(ElfLoadError::MappingFailed)?;

    let phys_offset = crate::boot::boot_info().physical_memory_offset;
    let frame_ptr = (phys_offset + private_frame.start_address().as_u64()).as_mut_ptr::<u8>();
    unsafe { core::ptr::write_bytes(frame_ptr, 0, 4096) };

    // Get the cached page (pins it; we must unpin after the memcpy).
    let cached_page =
        get_or_fill_page(inode, file_page_idx, fs).map_err(|_| ElfLoadError::MappingFailed)?;

    // Copy the 4 KiB page from the cache frame into our private frame via HHDM.
    let cache_phys = cached_page.frame().start_address().as_u64();
    let cache_ptr = (phys_offset + cache_phys).as_ptr::<u8>();
    unsafe {
        core::ptr::copy_nonoverlapping(cache_ptr, frame_ptr, 4096);
    }

    // Release the cache pin; the cache can now evict this page freely.
    cached_page.unpin();
    drop(cached_page);

    // ELF tail-zero: bytes past p_filesz within the last file page must read
    // as zero (they are the start of the BSS region that shares this page).
    if let Some(off) = zero_from {
        if off < 4096 {
            unsafe {
                core::ptr::write_bytes(frame_ptr.add(off), 0, 4096 - off);
            }
        }
    }

    memory_manager
        .map_address(page_addr, private_frame.start_address(), flags)
        .map_err(|_| ElfLoadError::MappingFailed)?;

    Ok(())
}

/// Prime the per-inode page cache with a contiguous file range using one
/// `fill_pages_bulk` call via the in-flight registry. The relocation loop in
/// `load_elf` would otherwise issue one synchronous single-page
/// `get_or_fill_page` per relocation target — under cold cache that's hundreds
/// of single-sector AHCI commands. One bulk read coalesces them into a single
/// NCQ command that completes in tens of milliseconds.
///
/// Best-effort: on bulk-read error, unsupported, or `owned_ops` overflow,
/// returns silently. The per-page `get_or_fill_page` calls in the relocation
/// loop serve as the fallback.
fn prefetch_file_pages(
    inode: &Arc<VfsInode>,
    fs: &Arc<dyn crate::fs::FileSystem + Send + Sync>,
    file_offset: u64,
    page_count: usize,
) {
    if page_count == 0 {
        return;
    }
    let Some(pc_ops) = fs.as_page_cache_ops() else {
        return;
    };

    let start_page = file_offset / 4096;
    let bytes = page_count * 4096;
    let ino = inode.ino;
    let byte_offset = file_offset as usize;

    // Use the bulk in-flight registry path. On failure (Err), the per-page
    // fallback in load_elf's relocation loop handles misses.
    let _ = page_fill::get_or_fill_bulk_async_sync(inode, start_page, page_count as u64, || {
        pc_ops.fill_pages_bulk(ino, byte_offset, bytes)
    });
}

/// Load an ELF binary via the inode page cache, building `VmaBacking::FileBacked`
/// VMAs for file-data ranges and `VmaBacking::Anonymous` VMAs for pure-BSS tails.
///
/// Returns `ElfLoadError::NoPageCache` when the inode's filesystem does not
/// support the page cache (e.g. FAT32, memfs). Callers map this to ENOEXEC.
pub fn load_elf(
    inode: &Arc<VfsInode>,
    path: &Path,
    memory_manager: &mut MemoryManager,
) -> Result<LoadedInfo, ElfLoadError> {
    // Resolve the filesystem for this inode.
    let fs = fs_by_mount_id(inode.mount_id).ok_or(ElfLoadError::MappingFailed)?;

    // Require page-cache support. Non-page-cache filesystems (FAT32, memfs) must
    // gain PageCacheOps before their binaries can be loaded via this path.
    if fs.as_page_cache_ops().is_none() {
        return Err(ElfLoadError::NoPageCache);
    }

    // --- Page-cache path ---
    //
    // Hand-parse the ehdr, program-header table, and section-header table
    // directly from the Elf64 on-disk layout. We avoid `elf::ElfBytes` here:
    // `minimal_parse` requires a single contiguous slice that spans both the
    // phdr and shdr tables, but linkers place shdrs at the end of the file
    // (edos-wm has e_shoff around 4.38 MiB), and we don't want to pull that
    // much into kernel memory just to enumerate headers.

    // Elf64_Ehdr field offsets (64-byte header, little-endian).
    const E_TYPE_OFF: usize = 0x10;
    const E_MACHINE_OFF: usize = 0x12;
    const E_ENTRY_OFF: usize = 0x18;
    const E_PHOFF_OFF: usize = 0x20;
    const E_SHOFF_OFF: usize = 0x28;
    const E_PHENTSIZE_OFF: usize = 0x36;
    const E_PHNUM_OFF: usize = 0x38;
    const E_SHENTSIZE_OFF: usize = 0x3A;
    const E_SHNUM_OFF: usize = 0x3C;

    const EI_CLASS: usize = 4;
    const EI_DATA: usize = 5;
    const ELFCLASS64: u8 = 2;
    const ELFDATA2LSB: u8 = 1;

    const EM_X86_64: u16 = 62;
    const ET_EXEC: u16 = 2;
    const ET_DYN: u16 = 3;
    const PT_LOAD: u32 = 1;
    const PT_TLS: u32 = 7;
    const PF_X: u32 = 1;
    const PF_W: u32 = 2;
    const PF_R: u32 = 4;
    const SHT_RELA: u32 = 4;

    const PHDR_SIZE: usize = 56;
    const SHDR_SIZE: usize = 64;

    let ehdr_bytes = read_file_range(path, 0, 64)?;
    let short = || ElfLoadError::MissingSegments;

    // e_ident: magic, then EI_CLASS=ELFCLASS64 and EI_DATA=ELFDATA2LSB. Without
    // this every field below is read out of whatever the file happens to hold.
    if ehdr_bytes.get(0..4) != Some(&[0x7f, b'E', b'L', b'F']) {
        return Err(ElfLoadError::NotAnElf);
    }
    if ehdr_bytes.get(EI_CLASS) != Some(&ELFCLASS64)
        || ehdr_bytes.get(EI_DATA) != Some(&ELFDATA2LSB)
    {
        return Err(ElfLoadError::UnsupportedArchitecture);
    }

    let e_type = le_u16(&ehdr_bytes, E_TYPE_OFF).ok_or_else(short)?;
    let e_machine = le_u16(&ehdr_bytes, E_MACHINE_OFF).ok_or_else(short)?;
    let e_entry = le_u64(&ehdr_bytes, E_ENTRY_OFF).ok_or_else(short)?;
    let e_phoff = le_u64(&ehdr_bytes, E_PHOFF_OFF).ok_or_else(short)?;
    let e_shoff = le_u64(&ehdr_bytes, E_SHOFF_OFF).ok_or_else(short)?;
    let e_phentsize = le_u16(&ehdr_bytes, E_PHENTSIZE_OFF).ok_or_else(short)?;
    let e_phnum = le_u16(&ehdr_bytes, E_PHNUM_OFF).ok_or_else(short)?;
    let e_shentsize = le_u16(&ehdr_bytes, E_SHENTSIZE_OFF).ok_or_else(short)?;
    let e_shnum = le_u16(&ehdr_bytes, E_SHNUM_OFF).ok_or_else(short)?;

    if e_machine != EM_X86_64 {
        return Err(ElfLoadError::UnsupportedArchitecture);
    }
    if e_entry == 0 {
        return Err(ElfLoadError::NoEntryPoint);
    }
    if e_phentsize as usize != PHDR_SIZE || e_shentsize as usize != SHDR_SIZE {
        return Err(ElfLoadError::UnsupportedArchitecture);
    }

    let load_base = match e_type {
        ET_EXEC => VirtAddr::new(0),
        ET_DYN => VirtAddr::new(0x400000),
        _ => return Err(ElfLoadError::UnsupportedArchitecture),
    };

    let base_addr = load_base;
    let mut max_addr = 0u64;
    let mut memory_regions: Vec<Vma> = Vec::new();
    let mut tls_template: Option<TlsTemplate> = None;
    let mut reloc_pages: BTreeSet<VirtAddr> = BTreeSet::new();
    let write_flags =
        PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE;
    // Candidate writable PT_LOAD segments. After SHT_RELA is parsed, exactly
    // one of these must contain all reloc targets; the rest are non-reloc
    // writable segments (e.g. a future GNU_RELRO + .data split). Phase 0
    // census shows current binaries always have exactly one writable PT_LOAD,
    // so writable_candidates.len() == 1 in practice today.
    struct WritableCandidate {
        vma_range: core::ops::Range<VirtAddr>,
        file_offset: u64,
    }
    let mut writable_candidates: Vec<WritableCandidate> = Vec::new();

    // Program header table (targeted read).
    let phdr_bytes_len = (e_phnum as u64) * (PHDR_SIZE as u64);
    let phdr_bytes = read_file_range(path, e_phoff, phdr_bytes_len)?;
    if phdr_bytes.len() < phdr_bytes_len as usize {
        return Err(ElfLoadError::MissingSegments);
    }

    // Task 2.4: Build FileBacked + BSS VMAs for each PT_LOAD.
    for i in 0..e_phnum as usize {
        let base = i * PHDR_SIZE;
        let p_type = le_u32(&phdr_bytes, base).ok_or_else(short)?;
        let p_flags = le_u32(&phdr_bytes, base + 4).ok_or_else(short)?;
        let p_offset = le_u64(&phdr_bytes, base + 8).ok_or_else(short)?;
        let p_vaddr = le_u64(&phdr_bytes, base + 16).ok_or_else(short)?;
        let p_filesz = le_u64(&phdr_bytes, base + 32).ok_or_else(short)?;
        let p_memsz = le_u64(&phdr_bytes, base + 40).ok_or_else(short)?;
        let p_align = le_u64(&phdr_bytes, base + 48).ok_or_else(short)?;

        if p_type == PT_LOAD {
            if p_memsz == 0 {
                continue;
            }

            // p_vaddr, p_memsz and p_filesz are attacker-controlled: any user can
            // spawn any file it can read. Everything below builds VirtAddrs and
            // VMA ranges out of them, so the segment is bounded to the user half
            // first — VirtAddr::new panics on a non-canonical address, and a VMA
            // reaching past USER_VA_END would map kernel space user-accessible.
            let seg_start = base_addr
                .as_u64()
                .checked_add(p_vaddr)
                .ok_or(ElfLoadError::InvalidSegment)?;
            let seg_end = seg_start
                .checked_add(p_memsz)
                .ok_or(ElfLoadError::InvalidSegment)?;
            // p_filesz > p_memsz would put the file-backed VMA past seg_end.
            if seg_end > USER_VA_END || p_filesz > p_memsz {
                return Err(ElfLoadError::InvalidSegment);
            }

            let vaddr = VirtAddr::new(seg_start);
            let page_aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !0xfff);
            let vaddr_offset = vaddr.as_u64() - page_aligned_vaddr.as_u64();

            // Linker invariant: p_offset % p_align == p_vaddr % p_align, so
            // file_offset = p_offset - vaddr_offset is page-aligned. A linker that
            // violates this would produce incorrect page-cache lookups.
            debug_assert!(
                (p_offset.wrapping_sub(vaddr_offset)) & 0xfff == 0,
                "ELF segment p_offset must share low bits with p_vaddr"
            );

            let file_offset = p_offset - vaddr_offset; // page-aligned file byte offset

            let mut prot = VmaProt::empty();
            if p_flags & PF_R != 0 {
                prot |= VmaProt::READ;
            }
            if p_flags & PF_W != 0 {
                prot |= VmaProt::WRITE;
            }
            if p_flags & PF_X != 0 {
                prot |= VmaProt::EXEC;
            }

            let writable_mapping = p_flags & PF_W != 0;

            // Number of pages that contain file data (including the partial last page).
            let file_last_page_end = align_up(vaddr_offset + p_filesz, 4096);
            let mem_last_page_end = align_up(vaddr_offset + p_memsz, 4096);
            let file_page_count = (file_last_page_end / 4096) as usize;

            // File-backed VMA covers all pages that touch file data.
            let file_vma_start = page_aligned_vaddr;
            let file_vma_end = page_aligned_vaddr + file_last_page_end;

            memory_regions.push(Vma {
                start: file_vma_start,
                end: file_vma_end,
                prot,
                flags: VmaFlags::PRIVATE | VmaFlags::LAZY,
                backing: VmaBacking::FileBacked {
                    inode: Arc::clone(inode),
                    file_offset,
                    shared: false,
                    writable_mapping,
                    pages: alloc::vec![None; file_page_count],
                },
            });

            // BSS VMA: only needed when BSS extends past the last file-data page.
            if mem_last_page_end > file_last_page_end {
                let bss_start = page_aligned_vaddr + file_last_page_end;
                let bss_end = page_aligned_vaddr + mem_last_page_end;
                memory_regions.push(Vma {
                    start: bss_start,
                    end: bss_end,
                    prot,
                    flags: VmaFlags::PRIVATE | VmaFlags::LAZY,
                    backing: VmaBacking::Anonymous,
                });
            }

            // Track every writable PT_LOAD as a candidate. The actual reloc-
            // bearing segment is identified after SHT_RELA is parsed (so the
            // assert correctly fires only when relocs span more than one).
            if writable_mapping {
                let vma_start = page_aligned_vaddr;
                let vma_end = page_aligned_vaddr + align_up(vaddr_offset + p_memsz, 4096);
                writable_candidates.push(WritableCandidate {
                    vma_range: vma_start..vma_end,
                    file_offset,
                });
            }

            // Pre-fault the last file page when p_filesz does not end on a page
            // boundary. Bytes past p_filesz within that page must read as zero
            // (they are the BSS-in-file-page region per the ELF spec), but the
            // page cache holds whatever the linker left there. We allocate a
            // private frame, copy the cache page, zero the tail, and map the
            // page into this process. Add to reloc_pages so the post-RelocTable
            // walk below can patch any RELATIVE entries targeting this page;
            // the lazy fault path will never fire for it because it is already
            // mapped.
            let tail_start = ((vaddr_offset + p_filesz) & 0xfff) as usize;
            if tail_start != 0 && p_filesz > 0 {
                let last_file_page_vma_offset = (file_page_count as u64 - 1) * 4096;
                let last_page_addr = page_aligned_vaddr + last_file_page_vma_offset;
                let last_file_page_idx = (file_offset + last_file_page_vma_offset) / 4096;
                prefault_elf_tail_page_from_cache(
                    memory_manager,
                    inode,
                    &fs,
                    last_file_page_idx,
                    last_page_addr,
                    write_flags,
                    Some(tail_start),
                )?;
                reloc_pages.insert(last_page_addr);
            }

            let segment_end = p_vaddr + p_memsz;
            max_addr = max_addr.max(segment_end + load_base.as_u64());
        } else if p_type == PT_TLS {
            // Task 2.5: TLS template via targeted read_file_range.
            if p_memsz == 0 {
                continue;
            }

            log_debug!("elf: TLS segment: filesz={} memsz={}", p_filesz, p_memsz);

            let init_data = if p_filesz == 0 {
                Vec::new()
            } else {
                read_file_range(path, p_offset, p_filesz)?
            };

            tls_template = Some(TlsTemplate {
                init_data,
                mem_size: p_memsz,
                align: p_align.max(1),
            });
        }
    }

    // Section header table (targeted read) — needed for reloc section walk.
    let shdr_bytes_len = (e_shnum as u64) * (SHDR_SIZE as u64);
    let shdr_bytes = read_file_range(path, e_shoff, shdr_bytes_len)?;
    if shdr_bytes.len() < shdr_bytes_len as usize {
        return Err(ElfLoadError::MissingSegments);
    }

    // Collect all R_X86_64_RELATIVE entries from SHT_RELA sections for
    // RelocTable construction (lazy fault-time application).
    let mut raw_relocs: Vec<(u32, i64)> = Vec::new();

    for i in 0..e_shnum as usize {
        let base = i * SHDR_SIZE;
        let sh_type = le_u32(&shdr_bytes, base + 4).ok_or_else(short)?;
        let sh_offset = le_u64(&shdr_bytes, base + 24).ok_or_else(short)?;
        let sh_size = le_u64(&shdr_bytes, base + 32).ok_or_else(short)?;

        match sh_type {
            SHT_RELA => {
                // Read relocation section data via targeted read_file_range.
                let rela_bytes = read_file_range(path, sh_offset, sh_size)?;

                // Each Elf64_Rela entry is 24 bytes: r_offset (8), r_info (8), r_addend (8).
                const RELA_SIZE: usize = 24;
                let count = rela_bytes.len() / RELA_SIZE;

                for j in 0..count {
                    let base = j * RELA_SIZE;
                    let r_offset = le_u64(&rela_bytes, base).ok_or_else(short)?;
                    let r_info = le_u64(&rela_bytes, base + 8).ok_or_else(short)?;
                    let r_addend = le_i64(&rela_bytes, base + 16).ok_or_else(short)?;
                    let reloc_type = (r_info & 0xffff_ffff) as u32;
                    // r_sym upper 32 bits: for RELATIVE, must be zero (no symbol ref).
                    let r_sym = (r_info >> 32) as u32;

                    match reloc_type {
                        // R_X86_64_RELATIVE (type 8)
                        8 => {
                            // A non-zero symbol index means this is a JUMP_SLOT or
                            // GLOB_DAT misclassified as RELATIVE, and the reloc
                            // table has no symbol to resolve it against.
                            if r_sym != 0 {
                                return Err(ElfLoadError::InvalidRelocation);
                            }

                            // The reloc table buckets targets by a u32 offset from
                            // the load base, so a target above 4 GiB would truncate
                            // into the wrong page.
                            if r_offset > u32::MAX as u64 {
                                return Err(ElfLoadError::InvalidRelocation);
                            }

                            raw_relocs.push((r_offset as u32, r_addend));
                        }
                        // JUMP_SLOT / GLOB_DAT: EDOS binaries are static-PIE, so
                        // dynamic symbol relocs have nothing to bind to.
                        6 | 7 => return Err(ElfLoadError::UnsupportedRelocation),
                        _ => {
                            println!("Unsupported relocation type: {}", reloc_type);
                        }
                    }
                }
            }
            // SHT_REL (9): unsupported on x86_64 (we use RELA only).
            9 => return Err(ElfLoadError::UnsupportedRelocation),
            // SHT_INIT_ARRAY (14): noted but not invoked here; the runtime
            // handles init array calls in _start.
            14 => {
                println!("INIT ARRAY FOUND: offset={:#x} size={}", sh_offset, sh_size);
            }
            _ => {}
        }
    }

    // Resolve the reloc-bearing writable PT_LOAD by intersecting the parsed
    // RELATIVE entries with the writable PT_LOAD candidates. Per Phase 0 census,
    // every current binary has exactly one such candidate. Two candidates that
    // both carry relocs, or none that carries them, describe a binary the lazy
    // reloc path cannot apply, so the load fails rather than the kernel.
    let (reloc_vma_range, reloc_skip_file_offset): (
        Option<core::ops::Range<VirtAddr>>,
        Option<u64>,
    ) = if raw_relocs.is_empty() {
        (None, None)
    } else {
        let mut matched: Option<&WritableCandidate> = None;
        for cand in writable_candidates.iter() {
            let start_off = cand.vma_range.start.as_u64() - load_base.as_u64();
            let end_off = cand.vma_range.end.as_u64() - load_base.as_u64();
            let any = raw_relocs.iter().any(|&(off, _)| {
                let off64 = off as u64;
                off64 >= start_off && off64 < end_off
            });
            if any {
                if matched.is_some() {
                    return Err(ElfLoadError::InvalidRelocation);
                }
                matched = Some(cand);
            }
        }
        let m = matched.ok_or(ElfLoadError::InvalidRelocation)?;
        (Some(m.vma_range.clone()), Some(m.file_offset))
    };

    // Pre-fetch PT_LOAD file pages into the per-inode page cache via bulk AHCI
    // reads. Skip the writable PT_LOAD that owns relocs (matched by
    // file_offset): those pages are private-on-fault and populated by the lazy
    // fault handler, not from a prefetch. All other segments (.text/.rodata and
    // non-reloc writable segments) are prefetched because they are read from
    // cache without modification.
    for region in &memory_regions {
        if let VmaBacking::FileBacked {
            file_offset, pages, ..
        } = &region.backing
        {
            if reloc_skip_file_offset == Some(*file_offset) {
                continue;
            }
            prefetch_file_pages(inode, &fs, *file_offset, pages.len());
        }
    }

    // Build the RelocTable for lazy fault-time application. A non-empty
    // raw_relocs implies reloc_vma_range was set above, since the resolution
    // returns an error otherwise.
    let built_reloc_table: Option<Arc<RelocTable>> = match reloc_vma_range.as_ref() {
        _ if raw_relocs.is_empty() => None,
        None => return Err(ElfLoadError::InvalidRelocation),
        Some(range) => {
            let vma_page_start = (range.start.as_u64() - load_base.as_u64()) as u32;
            let vma_page_count = ((range.end.as_u64() - range.start.as_u64()) / 4096) as usize;
            Some(RelocTable::build(
                raw_relocs,
                vma_page_start,
                vma_page_count,
            ))
        }
    };

    // Apply relocations to pages that were pre-faulted by
    // prefault_elf_tail_page_from_cache (the partial last file page that needs
    // zero-padding past p_filesz per the ELF spec). Those pages are already
    // mapped writable, so the lazy reloc fault path will never fire for them.
    // Without this, reloc targets that fall inside the partial last file page
    // (typical for .got sitting at the end of the writable PT_LOAD) remain
    // unrelocated and userspace dereferences a null-or-bogus address.
    if let Some(ref table) = built_reloc_table {
        use x86_64::structures::paging::mapper::TranslateResult;
        let phys_offset = crate::boot::boot_info().physical_memory_offset;
        for &page_addr in &reloc_pages {
            let page_offset = (page_addr.as_u64() - load_base.as_u64()) as u32;
            let TranslateResult::Mapped { frame, offset, .. } = memory_manager.translate(page_addr)
            else {
                panic!(
                    "lazy reloc: pre-faulted reloc page {:#x} not mapped",
                    page_addr.as_u64()
                );
            };
            let frame_virt = phys_offset + (frame.start_address() + offset).as_u64();
            unsafe {
                table.apply_relocs_to_page(frame_virt, page_offset, load_base.as_u64());
            }
        }
    }

    if let Some(ref table) = built_reloc_table {
        log_debug!(
            "elf: lazy reloc table: {} entries, {} pages",
            table.entry_count(),
            table.populated_buckets()
        );
    }

    if max_addr == 0 {
        max_addr = 0x10000000;
    }

    let actual_entry = VirtAddr::new(load_base.as_u64() + e_entry);

    log_debug!(
        "elf: loaded at {:#x}, entry={:#x} (file-backed path)",
        load_base.as_u64(),
        actual_entry.as_u64()
    );

    Ok(LoadedInfo {
        entry_point: actual_entry,
        heap_break: align_up(max_addr, 4096) + 0x10000,
        memory_regions,
        tls_template,
        reloc_table: built_reloc_table,
        reloc_vma_range,
        load_base: load_base.as_u64(),
    })
}
