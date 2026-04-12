use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use thiserror::Error;
use x86_64::{VirtAddr, align_up, structures::paging::PageTableFlags};

use crate::{
    fs::{
        api as fs_api,
        inode::VfsInode,
        path::Path,
        vfs::{fs_by_mount_id, get_or_fill_page},
    },
    log,
    memory::{
        frame_allocator::frame_allocator,
        mapper::MemoryManager,
        vma::{Vma, VmaBacking, VmaFlags, VmaProt},
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
}

/// Allocate a private frame, copy one page from the inode page cache into it,
/// and map it at `page_addr` with the given flags.
///
/// `zero_from` optionally zeroes bytes `[zero_from..4096]` after the memcpy.
/// Used for the last file-data page of a PT_LOAD whose `p_filesz` is not a
/// multiple of 4096: bytes past `p_filesz` are the start of BSS and must read
/// as zero per the ELF spec, but the cache page contains whatever the linker
/// left on disk (usually alignment junk).
///
/// The cache page is pinned only long enough for the memcpy; it is unpinned
/// before returning so the cache can evict it independently. The private frame
/// is fully independent of the cache.
fn eager_fault_elf_page_from_cache(
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

/// Allocate a zeroed private frame and map it writable at `page_addr`.
///
/// Used during relocation patching when the second page of a cross-page
/// 8-byte relocation entry lands in the BSS (Anonymous) VMA.
fn eager_fault_anon_page(
    mm: &mut MemoryManager,
    page_addr: VirtAddr,
    flags: PageTableFlags,
) -> Result<(), ElfLoadError> {
    use x86_64::structures::paging::FrameAllocator;

    let frame = frame_allocator()
        .allocate_frame()
        .ok_or(ElfLoadError::MappingFailed)?;

    let phys_offset = crate::boot::boot_info().physical_memory_offset;
    let frame_ptr = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
    unsafe { core::ptr::write_bytes(frame_ptr, 0, 4096) };

    mm.map_address(page_addr, frame.start_address(), flags)
        .map_err(|_| ElfLoadError::MappingFailed)?;

    Ok(())
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
    if ehdr_bytes.len() < 64 {
        return Err(ElfLoadError::MissingSegments);
    }

    let e_type = u16::from_le_bytes(ehdr_bytes[E_TYPE_OFF..E_TYPE_OFF + 2].try_into().unwrap());
    let e_machine = u16::from_le_bytes(
        ehdr_bytes[E_MACHINE_OFF..E_MACHINE_OFF + 2]
            .try_into()
            .unwrap(),
    );
    let e_entry = u64::from_le_bytes(ehdr_bytes[E_ENTRY_OFF..E_ENTRY_OFF + 8].try_into().unwrap());
    let e_phoff = u64::from_le_bytes(ehdr_bytes[E_PHOFF_OFF..E_PHOFF_OFF + 8].try_into().unwrap());
    let e_shoff = u64::from_le_bytes(ehdr_bytes[E_SHOFF_OFF..E_SHOFF_OFF + 8].try_into().unwrap());
    let e_phentsize = u16::from_le_bytes(
        ehdr_bytes[E_PHENTSIZE_OFF..E_PHENTSIZE_OFF + 2]
            .try_into()
            .unwrap(),
    );
    let e_phnum = u16::from_le_bytes(ehdr_bytes[E_PHNUM_OFF..E_PHNUM_OFF + 2].try_into().unwrap());
    let e_shentsize = u16::from_le_bytes(
        ehdr_bytes[E_SHENTSIZE_OFF..E_SHENTSIZE_OFF + 2]
            .try_into()
            .unwrap(),
    );
    let e_shnum = u16::from_le_bytes(ehdr_bytes[E_SHNUM_OFF..E_SHNUM_OFF + 2].try_into().unwrap());

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

    // Program header table (targeted read).
    let phdr_bytes_len = (e_phnum as u64) * (PHDR_SIZE as u64);
    let phdr_bytes = read_file_range(path, e_phoff, phdr_bytes_len)?;
    if phdr_bytes.len() < phdr_bytes_len as usize {
        return Err(ElfLoadError::MissingSegments);
    }

    // Task 2.4: Build FileBacked + BSS VMAs for each PT_LOAD.
    for i in 0..e_phnum as usize {
        let ph = &phdr_bytes[i * PHDR_SIZE..(i + 1) * PHDR_SIZE];
        let p_type = u32::from_le_bytes(ph[0..4].try_into().unwrap());
        let p_flags = u32::from_le_bytes(ph[4..8].try_into().unwrap());
        let p_offset = u64::from_le_bytes(ph[8..16].try_into().unwrap());
        let p_vaddr = u64::from_le_bytes(ph[16..24].try_into().unwrap());
        let p_filesz = u64::from_le_bytes(ph[32..40].try_into().unwrap());
        let p_memsz = u64::from_le_bytes(ph[40..48].try_into().unwrap());
        let p_align = u64::from_le_bytes(ph[48..56].try_into().unwrap());

        if p_type == PT_LOAD {
            if p_memsz == 0 {
                continue;
            }

            let vaddr = base_addr + p_vaddr;
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

            // Pre-fault the last file page when p_filesz does not end on a page
            // boundary. Bytes past p_filesz within that page must read as zero
            // (they are the BSS-in-file-page region per the ELF spec), but the
            // page cache holds whatever the linker left there. We allocate a
            // private frame, copy the cache page, zero the tail, and pin the
            // page into this process. Add to reloc_pages so the relocation
            // loop won't re-fault it; the final change_flags pass tightens
            // its PTE to the segment's real permissions.
            let tail_start = ((vaddr_offset + p_filesz) & 0xfff) as usize;
            if tail_start != 0 && p_filesz > 0 {
                let last_file_page_vma_offset = (file_page_count as u64 - 1) * 4096;
                let last_page_addr = page_aligned_vaddr + last_file_page_vma_offset;
                let last_file_page_idx = (file_offset + last_file_page_vma_offset) / 4096;
                eager_fault_elf_page_from_cache(
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

            log!("elf: TLS segment: filesz={} memsz={}", p_filesz, p_memsz);

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

    // Task 2.7: Relocation loop using the page-cache helper.
    // Section header table (targeted read).
    let shdr_bytes_len = (e_shnum as u64) * (SHDR_SIZE as u64);
    let shdr_bytes = read_file_range(path, e_shoff, shdr_bytes_len)?;
    if shdr_bytes.len() < shdr_bytes_len as usize {
        return Err(ElfLoadError::MissingSegments);
    }

    for i in 0..e_shnum as usize {
        let sh = &shdr_bytes[i * SHDR_SIZE..(i + 1) * SHDR_SIZE];
        let sh_type = u32::from_le_bytes(sh[4..8].try_into().unwrap());
        let sh_offset = u64::from_le_bytes(sh[24..32].try_into().unwrap());
        let sh_size = u64::from_le_bytes(sh[32..40].try_into().unwrap());

        match sh_type {
            SHT_RELA => {
                // Read relocation section data via targeted read_file_range.
                let rela_bytes = read_file_range(path, sh_offset, sh_size)?;

                // Each Elf64_Rela entry is 24 bytes: r_offset (8), r_info (8), r_addend (8).
                const RELA_SIZE: usize = 24;
                let count = rela_bytes.len() / RELA_SIZE;

                for i in 0..count {
                    let entry = &rela_bytes[i * RELA_SIZE..(i + 1) * RELA_SIZE];
                    let r_offset = u64::from_le_bytes(entry[0..8].try_into().unwrap());
                    let r_info = u64::from_le_bytes(entry[8..16].try_into().unwrap());
                    let r_addend = i64::from_le_bytes(entry[16..24].try_into().unwrap());
                    let reloc_type = (r_info & 0xffff_ffff) as u32;

                    match reloc_type {
                        // R_X86_64_RELATIVE
                        8 => {
                            let reloc_addr = base_addr + r_offset;
                            let first_page = VirtAddr::new(reloc_addr.as_u64() & !0xfff);
                            // An 8-byte patch may straddle a page boundary.
                            let last_page = VirtAddr::new((reloc_addr.as_u64() + 7) & !0xfff);

                            for &page_addr in &[first_page, last_page] {
                                if reloc_pages.contains(&page_addr) {
                                    continue;
                                }

                                if let Some(region) =
                                    memory_regions.iter().find(|r| r.contains(page_addr))
                                {
                                    match &region.backing {
                                        VmaBacking::FileBacked {
                                            file_offset: vma_file_offset,
                                            ..
                                        } => {
                                            // page index into the file
                                            let page_byte_offset =
                                                page_addr.as_u64() - region.start.as_u64();
                                            let file_page_idx =
                                                (vma_file_offset + page_byte_offset) / 4096;
                                            eager_fault_elf_page_from_cache(
                                                memory_manager,
                                                inode,
                                                &fs,
                                                file_page_idx,
                                                page_addr,
                                                write_flags,
                                                None,
                                            )?;
                                        }
                                        VmaBacking::Anonymous => {
                                            // BSS page: allocate zeroed private frame.
                                            eager_fault_anon_page(
                                                memory_manager,
                                                page_addr,
                                                write_flags,
                                            )?;
                                        }
                                        _ => {}
                                    }
                                    reloc_pages.insert(page_addr);
                                }
                            }

                            let value = base_addr.as_u64().wrapping_add(r_addend as u64);
                            memory_manager.write_val_to_user(reloc_addr, value);
                        }
                        _ => {
                            println!("Unsupported relocation type: {}", reloc_type);
                        }
                    }
                }
            }
            // SHT_REL (9): unsupported on x86_64 (we use RELA only).
            9 => {
                panic!("REL relocation unsupported");
            }
            // SHT_INIT_ARRAY (14): noted but not invoked here; the runtime
            // handles init array calls in _start.
            14 => {
                println!("INIT ARRAY FOUND: offset={:#x} size={}", sh_offset, sh_size);
            }
            _ => {}
        }
    }

    // Task 2.8: Tighten permissions on eagerly-faulted relocation pages.
    // Pages were mapped WRITABLE for patching; set final perms from the
    // containing VMA's VmaProt. Works for both FileBacked and Anonymous VMAs.
    for &page_addr in &reloc_pages {
        if let Some(region) = memory_regions.iter().find(|r| r.contains(page_addr)) {
            let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if region.prot.contains(VmaProt::WRITE) {
                flags |= PageTableFlags::WRITABLE;
            }
            if !region.prot.contains(VmaProt::EXEC) {
                flags |= PageTableFlags::NO_EXECUTE;
            }
            log!(
                "elf: reloc page {:#x} in {:?} VMA [{:#x}, {:#x}), final flags {:?}",
                page_addr.as_u64(),
                region.backing,
                region.start.as_u64(),
                region.end.as_u64(),
                flags
            );
            memory_manager
                .change_flags(page_addr, 4096, flags)
                .map_err(|e| {
                    println!("ELF: Failed to update flags on reloc page: {:?}", e);
                    ElfLoadError::MappingFailed
                })?;
        }
    }

    if max_addr == 0 {
        max_addr = 0x10000000;
    }

    let actual_entry = VirtAddr::new(load_base.as_u64() + e_entry);

    log!(
        "elf: loaded at {:#x}, entry={:#x} (file-backed path)",
        load_base.as_u64(),
        actual_entry.as_u64()
    );

    Ok(LoadedInfo {
        entry_point: actual_entry,
        heap_break: align_up(max_addr, 4096) + 0x10000,
        memory_regions,
        tls_template,
    })
}
