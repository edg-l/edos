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

impl TlsTemplate {
    /// What an image with no `PT_TLS` gets.
    ///
    /// A thread has a control block whether or not its program declares a
    /// thread-local, because the runtime keeps its own state there and finds it
    /// at a fixed offset from `%fs`. Treating a missing `PT_TLS` as "no TLS
    /// region" would leave `%fs` at zero for such an image and turn that offset
    /// into a null dereference.
    pub fn empty() -> Self {
        Self {
            init_data: Vec::new(),
            mem_size: 0,
            align: 1,
        }
    }
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
    /// User address of the program header table, for `AT_PHDR`. `None` when the
    /// image maps no segment covering it, which leaves a dynamic linker unable
    /// to find the main image's headers.
    pub phdr_vaddr: Option<u64>,
    /// `e_phentsize` and `e_phnum`, for `AT_PHENT` and `AT_PHNUM`.
    pub phentsize: u16,
    pub phnum: u16,
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
    if let Some(off) = zero_from
        && off < 4096
    {
        unsafe {
            core::ptr::write_bytes(frame_ptr.add(off), 0, 4096 - off);
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

/// Elf64 on-disk field offsets, little-endian throughout.
///
/// The loader hand-parses these rather than using `elf::ElfBytes`:
/// `minimal_parse` wants one contiguous slice spanning both the phdr and shdr
/// tables, and linkers place shdrs at the end of the file (`edos-wm`'s
/// `e_shoff` is around 4.38 MiB), so satisfying it would mean pulling the whole
/// binary into kernel memory just to enumerate headers.
mod elf64 {
    pub const E_TYPE_OFF: usize = 0x10;
    pub const E_MACHINE_OFF: usize = 0x12;
    pub const E_ENTRY_OFF: usize = 0x18;
    pub const E_PHOFF_OFF: usize = 0x20;
    pub const E_SHOFF_OFF: usize = 0x28;
    pub const E_PHENTSIZE_OFF: usize = 0x36;
    pub const E_PHNUM_OFF: usize = 0x38;
    pub const E_SHENTSIZE_OFF: usize = 0x3A;
    pub const E_SHNUM_OFF: usize = 0x3C;

    pub const EI_CLASS: usize = 4;
    pub const EI_DATA: usize = 5;
    pub const ELFCLASS64: u8 = 2;
    pub const ELFDATA2LSB: u8 = 1;

    pub const EM_X86_64: u16 = 62;
    pub const ET_EXEC: u16 = 2;
    pub const ET_DYN: u16 = 3;
    pub const PT_LOAD: u32 = 1;
    pub const PT_PHDR: u32 = 6;
    pub const PT_TLS: u32 = 7;
    pub const PF_X: u32 = 1;
    pub const PF_W: u32 = 2;
    pub const PF_R: u32 = 4;
    pub const SHT_RELA: u32 = 4;
    pub const SHT_REL: u32 = 9;
    pub const SHT_INIT_ARRAY: u32 = 14;
    /// `R_X86_64_RELATIVE`, the only relocation a static-PIE image needs.
    pub const R_X86_64_RELATIVE: u32 = 8;
    pub const R_X86_64_GLOB_DAT: u32 = 6;
    pub const R_X86_64_JUMP_SLOT: u32 = 7;

    pub const EHDR_SIZE: u64 = 64;
    pub const PHDR_SIZE: usize = 56;
    pub const SHDR_SIZE: usize = 64;
    pub const RELA_SIZE: usize = 24;
}

/// A header field that does not lie wholly within the bytes read for it.
fn short() -> ElfLoadError {
    ElfLoadError::MissingSegments
}

/// The `Elf64_Ehdr` fields the loader acts on, once the identification bytes,
/// class, machine, object type and header entry sizes have been checked.
struct ElfHeader {
    /// 0 for `ET_EXEC`, 0x400000 for a static-PIE `ET_DYN`.
    load_base: u64,
    entry: u64,
    phoff: u64,
    shoff: u64,
    phentsize: u16,
    phnum: u16,
    shnum: u16,
}

/// One `PT_LOAD`, validated to lie wholly within the user half and to describe
/// a coherent file range.
///
/// Every address here is already resolved against the load base and page
/// aligned, so the map step builds VMAs without re-deriving anything from an
/// attacker-controlled field.
struct LoadSegment {
    /// Page-aligned virtual address the segment's first page maps at.
    vma_start: VirtAddr,
    /// Byte offset of `p_vaddr` within that first page.
    vaddr_offset: u64,
    /// Page-aligned file byte offset that first page holds.
    file_offset: u64,
    prot: VmaProt,
    writable: bool,
    filesz: u64,
    /// Offset past `vma_start` where file data ends, rounded up to a page.
    file_end: u64,
    /// Offset past `vma_start` where the segment ends, rounded up to a page.
    mem_end: u64,
}

impl LoadSegment {
    /// Pages that contain file data, including a partial last one.
    fn file_page_count(&self) -> usize {
        (self.file_end / 4096) as usize
    }

    /// Offset into the last file-data page where `p_filesz` ends, or `None`
    /// when file data ends on a page boundary.
    ///
    /// Bytes past that offset are the start of BSS and must read as zero per
    /// the ELF spec, but the page cache holds whatever the linker left on disk.
    fn tail_zero_from(&self) -> Option<usize> {
        let off = ((self.vaddr_offset + self.filesz) & 0xfff) as usize;
        (off != 0 && self.filesz > 0).then_some(off)
    }
}

/// Everything the map step needs, parsed out of attacker-controlled bytes and
/// validated. Past this point no header field is read again.
struct ElfImage {
    load_base: u64,
    entry: VirtAddr,
    segments: Vec<LoadSegment>,
    tls_template: Option<TlsTemplate>,
    /// `R_X86_64_RELATIVE` entries as (offset from the load base, addend).
    relocs: Vec<(u32, i64)>,
    /// User address of the program header table, for `AT_PHDR`.
    phdr_vaddr: Option<u64>,
    phentsize: u16,
    phnum: u16,
    /// Highest address any `PT_LOAD` reaches; the heap starts past it.
    max_addr: u64,
}

/// Parse and validate the ELF header.
fn parse_ehdr(path: &Path) -> Result<ElfHeader, ElfLoadError> {
    use elf64::*;

    let bytes = read_file_range(path, 0, EHDR_SIZE)?;

    // e_ident: magic, then EI_CLASS=ELFCLASS64 and EI_DATA=ELFDATA2LSB. Without
    // this every field below is read out of whatever the file happens to hold.
    if bytes.get(0..4) != Some(&[0x7f, b'E', b'L', b'F']) {
        return Err(ElfLoadError::NotAnElf);
    }
    if bytes.get(EI_CLASS) != Some(&ELFCLASS64) || bytes.get(EI_DATA) != Some(&ELFDATA2LSB) {
        return Err(ElfLoadError::UnsupportedArchitecture);
    }

    let e_type = le_u16(&bytes, E_TYPE_OFF).ok_or_else(short)?;
    let e_machine = le_u16(&bytes, E_MACHINE_OFF).ok_or_else(short)?;
    let entry = le_u64(&bytes, E_ENTRY_OFF).ok_or_else(short)?;
    let phoff = le_u64(&bytes, E_PHOFF_OFF).ok_or_else(short)?;
    let shoff = le_u64(&bytes, E_SHOFF_OFF).ok_or_else(short)?;
    let phentsize = le_u16(&bytes, E_PHENTSIZE_OFF).ok_or_else(short)?;
    let phnum = le_u16(&bytes, E_PHNUM_OFF).ok_or_else(short)?;
    let shentsize = le_u16(&bytes, E_SHENTSIZE_OFF).ok_or_else(short)?;
    let shnum = le_u16(&bytes, E_SHNUM_OFF).ok_or_else(short)?;

    if e_machine != EM_X86_64 {
        return Err(ElfLoadError::UnsupportedArchitecture);
    }
    if entry == 0 {
        return Err(ElfLoadError::NoEntryPoint);
    }
    if phentsize as usize != PHDR_SIZE || shentsize as usize != SHDR_SIZE {
        return Err(ElfLoadError::UnsupportedArchitecture);
    }

    let load_base = match e_type {
        ET_EXEC => 0,
        ET_DYN => 0x400000,
        _ => return Err(ElfLoadError::UnsupportedArchitecture),
    };

    Ok(ElfHeader {
        load_base,
        entry,
        phoff,
        shoff,
        phentsize,
        phnum,
        shnum,
    })
}

/// Validate one `PT_LOAD` into a `LoadSegment`.
///
/// `p_vaddr`, `p_memsz` and `p_filesz` are attacker-controlled: any user can
/// spawn any file it can read, and the map step turns these into `VirtAddr`s
/// and VMA ranges. `VirtAddr::new` panics on a non-canonical address, and a VMA
/// reaching past `USER_VA_END` would map kernel space user-accessible, so the
/// segment is bounded to the user half here and nowhere else.
fn validate_load_segment(
    load_base: u64,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_filesz: u64,
    p_memsz: u64,
) -> Result<LoadSegment, ElfLoadError> {
    use elf64::*;

    let seg_start = load_base
        .checked_add(p_vaddr)
        .ok_or(ElfLoadError::InvalidSegment)?;
    let seg_end = seg_start
        .checked_add(p_memsz)
        .ok_or(ElfLoadError::InvalidSegment)?;
    // p_filesz > p_memsz would put the file-backed VMA past seg_end.
    if seg_end > USER_VA_END || p_filesz > p_memsz {
        return Err(ElfLoadError::InvalidSegment);
    }

    let vma_start = VirtAddr::new(seg_start & !0xfff);
    let vaddr_offset = seg_start - vma_start.as_u64();

    // Linker invariant: p_offset % p_align == p_vaddr % p_align, so
    // file_offset = p_offset - vaddr_offset is page-aligned. A linker that
    // violates this would produce incorrect page-cache lookups.
    debug_assert!(
        (p_offset.wrapping_sub(vaddr_offset)) & 0xfff == 0,
        "ELF segment p_offset must share low bits with p_vaddr"
    );

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

    Ok(LoadSegment {
        vma_start,
        vaddr_offset,
        file_offset: p_offset - vaddr_offset,
        prot,
        writable: p_flags & PF_W != 0,
        filesz: p_filesz,
        file_end: align_up(vaddr_offset + p_filesz, 4096),
        mem_end: align_up(vaddr_offset + p_memsz, 4096),
    })
}

/// Collect the `R_X86_64_RELATIVE` entries of every `SHT_RELA` section.
///
/// Returns `UnsupportedRelocation` for a relocation kind EDOS does not
/// implement and `InvalidRelocation` for one whose fields are incoherent.
fn parse_relocs(path: &Path, hdr: &ElfHeader) -> Result<Vec<(u32, i64)>, ElfLoadError> {
    use elf64::*;

    let shdr_bytes_len = (hdr.shnum as u64) * (SHDR_SIZE as u64);
    let shdr_bytes = read_file_range(path, hdr.shoff, shdr_bytes_len)?;
    if shdr_bytes.len() < shdr_bytes_len as usize {
        return Err(ElfLoadError::MissingSegments);
    }

    let mut relocs: Vec<(u32, i64)> = Vec::new();

    for i in 0..hdr.shnum as usize {
        let base = i * SHDR_SIZE;
        let sh_type = le_u32(&shdr_bytes, base + 4).ok_or_else(short)?;
        let sh_offset = le_u64(&shdr_bytes, base + 24).ok_or_else(short)?;
        let sh_size = le_u64(&shdr_bytes, base + 32).ok_or_else(short)?;

        match sh_type {
            SHT_RELA => {
                let rela_bytes = read_file_range(path, sh_offset, sh_size)?;
                for j in 0..rela_bytes.len() / RELA_SIZE {
                    let base = j * RELA_SIZE;
                    let r_offset = le_u64(&rela_bytes, base).ok_or_else(short)?;
                    let r_info = le_u64(&rela_bytes, base + 8).ok_or_else(short)?;
                    let r_addend = le_i64(&rela_bytes, base + 16).ok_or_else(short)?;
                    let reloc_type = (r_info & 0xffff_ffff) as u32;
                    let r_sym = (r_info >> 32) as u32;

                    match reloc_type {
                        R_X86_64_RELATIVE => {
                            // A non-zero symbol index means this is a JUMP_SLOT
                            // or GLOB_DAT misclassified as RELATIVE, and the
                            // reloc table has no symbol to resolve it against.
                            if r_sym != 0 {
                                return Err(ElfLoadError::InvalidRelocation);
                            }
                            // The reloc table buckets targets by a u32 offset
                            // from the load base, so a target above 4 GiB would
                            // truncate into the wrong page.
                            if r_offset > u32::MAX as u64 {
                                return Err(ElfLoadError::InvalidRelocation);
                            }
                            relocs.push((r_offset as u32, r_addend));
                        }
                        // EDOS binaries are static-PIE, so dynamic symbol
                        // relocs have nothing to bind to.
                        R_X86_64_GLOB_DAT | R_X86_64_JUMP_SLOT => {
                            return Err(ElfLoadError::UnsupportedRelocation);
                        }
                        _ => println!("Unsupported relocation type: {}", reloc_type),
                    }
                }
            }
            // Unsupported on x86_64; the psABI uses RELA only.
            SHT_REL => return Err(ElfLoadError::UnsupportedRelocation),
            // Noted but not invoked here; the runtime calls the init array
            // from _start.
            SHT_INIT_ARRAY => {
                println!("INIT ARRAY FOUND: offset={:#x} size={}", sh_offset, sh_size)
            }
            _ => {}
        }
    }

    Ok(relocs)
}

/// Parse an ELF image into a validated description: segments bounded to the
/// user half, a TLS template, the relocations, and the addresses the auxiliary
/// vector needs. Nothing is mapped and no process state is touched.
fn parse_image(path: &Path) -> Result<ElfImage, ElfLoadError> {
    use elf64::*;

    let hdr = parse_ehdr(path)?;

    let phdr_bytes_len = (hdr.phnum as u64) * (PHDR_SIZE as u64);
    let phdr_bytes = read_file_range(path, hdr.phoff, phdr_bytes_len)?;
    if phdr_bytes.len() < phdr_bytes_len as usize {
        return Err(ElfLoadError::MissingSegments);
    }

    // AT_PHDR. The psABI leaves the program header table's user address to the
    // kernel, and it is the only channel a dynamic linker has for finding the
    // main image's headers. `PT_PHDR` names the address outright; without one
    // the table is wherever the `PT_LOAD` covering `e_phoff` maps it.
    let mut phdr_vaddr: Option<u64> = None;
    for i in 0..hdr.phnum as usize {
        let base = i * PHDR_SIZE;
        let p_type = le_u32(&phdr_bytes, base).ok_or_else(short)?;
        let p_offset = le_u64(&phdr_bytes, base + 8).ok_or_else(short)?;
        let p_vaddr = le_u64(&phdr_bytes, base + 16).ok_or_else(short)?;
        let p_filesz = le_u64(&phdr_bytes, base + 32).ok_or_else(short)?;

        if p_type == PT_PHDR {
            phdr_vaddr = Some(hdr.load_base + p_vaddr);
            break;
        }
        if p_type == PT_LOAD
            && hdr.phoff >= p_offset
            && hdr.phoff + phdr_bytes_len <= p_offset + p_filesz
        {
            phdr_vaddr = Some(hdr.load_base + p_vaddr + (hdr.phoff - p_offset));
        }
    }

    let mut segments: Vec<LoadSegment> = Vec::new();
    let mut tls_template: Option<TlsTemplate> = None;
    let mut max_addr = 0u64;

    for i in 0..hdr.phnum as usize {
        let base = i * PHDR_SIZE;
        let p_type = le_u32(&phdr_bytes, base).ok_or_else(short)?;
        let p_flags = le_u32(&phdr_bytes, base + 4).ok_or_else(short)?;
        let p_offset = le_u64(&phdr_bytes, base + 8).ok_or_else(short)?;
        let p_vaddr = le_u64(&phdr_bytes, base + 16).ok_or_else(short)?;
        let p_filesz = le_u64(&phdr_bytes, base + 32).ok_or_else(short)?;
        let p_memsz = le_u64(&phdr_bytes, base + 40).ok_or_else(short)?;
        let p_align = le_u64(&phdr_bytes, base + 48).ok_or_else(short)?;

        if p_memsz == 0 {
            continue;
        }

        match p_type {
            PT_LOAD => {
                segments.push(validate_load_segment(
                    hdr.load_base,
                    p_flags,
                    p_offset,
                    p_vaddr,
                    p_filesz,
                    p_memsz,
                )?);
                max_addr = max_addr.max(hdr.load_base + p_vaddr + p_memsz);
            }
            PT_TLS => {
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
            _ => {}
        }
    }

    let relocs = parse_relocs(path, &hdr)?;

    Ok(ElfImage {
        load_base: hdr.load_base,
        entry: VirtAddr::new(hdr.load_base + hdr.entry),
        segments,
        tls_template,
        relocs,
        phdr_vaddr,
        phentsize: hdr.phentsize,
        phnum: hdr.phnum,
        max_addr: if max_addr == 0 { 0x10000000 } else { max_addr },
    })
}

/// The writable `PT_LOAD` that carries the image's relocations, resolved by
/// intersecting the parsed targets with the writable segments.
///
/// Every current binary has exactly one writable `PT_LOAD`, so there is one
/// candidate. Two that both carry relocs, or none that does, describe a binary
/// the lazy reloc path cannot apply, so the load fails rather than the kernel.
fn resolve_reloc_segment(image: &ElfImage) -> Result<Option<usize>, ElfLoadError> {
    if image.relocs.is_empty() {
        return Ok(None);
    }

    let mut matched: Option<usize> = None;
    for (i, seg) in image.segments.iter().enumerate() {
        if !seg.writable {
            continue;
        }
        let start_off = seg.vma_start.as_u64() - image.load_base;
        let end_off = start_off + seg.mem_end;
        let any = image
            .relocs
            .iter()
            .any(|&(off, _)| (off as u64) >= start_off && (off as u64) < end_off);
        if any {
            if matched.is_some() {
                return Err(ElfLoadError::InvalidRelocation);
            }
            matched = Some(i);
        }
    }

    matched.ok_or(ElfLoadError::InvalidRelocation).map(Some)
}

/// Turn a validated image into the process's VMAs and relocation table.
///
/// Builds a `VmaBacking::FileBacked` VMA for each segment's file-data range and
/// a `VmaBacking::Anonymous` one for its pure-BSS tail, pre-faults the partial
/// last file page where one exists, and prefetches every segment the lazy fault
/// path will read straight from the page cache.
fn map_image(
    mut image: ElfImage,
    inode: &Arc<VfsInode>,
    fs: &Arc<dyn crate::fs::FileSystem + Send + Sync>,
    memory_manager: &mut MemoryManager,
) -> Result<LoadedInfo, ElfLoadError> {
    let reloc_seg = resolve_reloc_segment(&image)?.map(|i| &image.segments[i]);
    let reloc_vma_range = reloc_seg.map(|s| s.vma_start..s.vma_start + s.mem_end);
    let reloc_skip_file_offset = reloc_seg.map(|s| s.file_offset);
    let reloc_table_shape = reloc_seg.map(|s| {
        (
            (s.vma_start.as_u64() - image.load_base) as u32,
            (s.mem_end / 4096) as usize,
        )
    });

    let write_flags =
        PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE;
    let mut memory_regions: Vec<Vma> = Vec::new();
    // Pages pre-faulted below. The lazy reloc fault path can never fire for
    // them, so any relocation targeting one is applied by hand afterwards.
    let mut reloc_pages: BTreeSet<VirtAddr> = BTreeSet::new();

    for seg in &image.segments {
        memory_regions.push(Vma {
            start: seg.vma_start,
            end: seg.vma_start + seg.file_end,
            prot: seg.prot,
            flags: VmaFlags::PRIVATE | VmaFlags::LAZY,
            backing: VmaBacking::FileBacked {
                inode: Arc::clone(inode),
                file_offset: seg.file_offset,
                shared: false,
                writable_mapping: seg.writable,
                pages: alloc::vec![None; seg.file_page_count()],
            },
        });

        // BSS VMA: only needed when BSS extends past the last file-data page.
        if seg.mem_end > seg.file_end {
            memory_regions.push(Vma {
                start: seg.vma_start + seg.file_end,
                end: seg.vma_start + seg.mem_end,
                prot: seg.prot,
                flags: VmaFlags::PRIVATE | VmaFlags::LAZY,
                backing: VmaBacking::Anonymous,
            });
        }

        // Pre-fault the last file page when p_filesz does not end on a page
        // boundary, so the bytes past it read as zero rather than as the
        // alignment junk the linker left in the cache page.
        if let Some(tail_start) = seg.tail_zero_from() {
            let last_page_offset = (seg.file_page_count() as u64 - 1) * 4096;
            let last_page_addr = seg.vma_start + last_page_offset;
            prefault_elf_tail_page_from_cache(
                memory_manager,
                inode,
                fs,
                (seg.file_offset + last_page_offset) / 4096,
                last_page_addr,
                write_flags,
                Some(tail_start),
            )?;
            reloc_pages.insert(last_page_addr);
        }
    }

    // Pre-fetch PT_LOAD file pages into the per-inode page cache via bulk AHCI
    // reads. Skip the writable PT_LOAD that owns relocs: those pages are
    // private-on-fault and populated by the lazy fault handler, not from a
    // prefetch. All other segments (.text/.rodata and non-reloc writable
    // segments) are read from cache without modification.
    for region in &memory_regions {
        if let VmaBacking::FileBacked {
            file_offset, pages, ..
        } = &region.backing
            && reloc_skip_file_offset != Some(*file_offset)
        {
            prefetch_file_pages(inode, fs, *file_offset, pages.len());
        }
    }

    // Build the RelocTable for lazy fault-time application.
    let reloc_table = reloc_table_shape.map(|(vma_page_start, vma_page_count)| {
        RelocTable::build(
            core::mem::take(&mut image.relocs),
            vma_page_start,
            vma_page_count,
        )
    });

    // Apply relocations to the pre-faulted pages. Without this, reloc targets
    // that fall inside the partial last file page (typical for .got sitting at
    // the end of the writable PT_LOAD) stay unrelocated and userspace
    // dereferences a null-or-bogus address.
    if let Some(ref table) = reloc_table {
        use x86_64::structures::paging::mapper::TranslateResult;
        let phys_offset = crate::boot::boot_info().physical_memory_offset;
        for &page_addr in &reloc_pages {
            let page_offset = (page_addr.as_u64() - image.load_base) as u32;
            let TranslateResult::Mapped { frame, offset, .. } = memory_manager.translate(page_addr)
            else {
                panic!(
                    "lazy reloc: pre-faulted reloc page {:#x} not mapped",
                    page_addr.as_u64()
                );
            };
            let frame_virt = phys_offset + (frame.start_address() + offset).as_u64();
            unsafe {
                table.apply_relocs_to_page(frame_virt, page_offset, image.load_base);
            }
        }

        log_debug!(
            "elf: lazy reloc table: {} entries, {} pages",
            table.entry_count(),
            table.populated_buckets()
        );
    }

    log_debug!(
        "elf: loaded at {:#x}, entry={:#x} (file-backed path)",
        image.load_base,
        image.entry.as_u64()
    );

    Ok(LoadedInfo {
        entry_point: image.entry,
        heap_break: align_up(image.max_addr, 4096) + 0x10000,
        memory_regions,
        tls_template: image.tls_template,
        reloc_table,
        reloc_vma_range,
        load_base: image.load_base,
        phdr_vaddr: image.phdr_vaddr,
        phentsize: image.phentsize,
        phnum: image.phnum,
    })
}

/// Load an ELF binary via the inode page cache, building `VmaBacking::FileBacked`
/// VMAs for file-data ranges and `VmaBacking::Anonymous` VMAs for pure-BSS tails.
///
/// Parsing and mapping are separate steps: `parse_image` validates every
/// attacker-controlled field into an `ElfImage` and touches no process state,
/// and `map_image` consumes that description without re-reading a header. A
/// malformed binary therefore fails before anything is mapped.
///
/// Returns `ElfLoadError::NoPageCache` when the inode's filesystem does not
/// support the page cache (e.g. FAT32, memfs). Callers map this to ENOEXEC.
pub fn load_elf(
    inode: &Arc<VfsInode>,
    path: &Path,
    memory_manager: &mut MemoryManager,
) -> Result<LoadedInfo, ElfLoadError> {
    let fs = fs_by_mount_id(inode.mount_id).ok_or(ElfLoadError::MappingFailed)?;

    // Non-page-cache filesystems (FAT32, memfs) must gain PageCacheOps before
    // their binaries can be loaded via this path.
    if fs.as_page_cache_ops().is_none() {
        return Err(ElfLoadError::NoPageCache);
    }

    map_image(parse_image(path)?, inode, &fs, memory_manager)
}
