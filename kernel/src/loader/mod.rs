use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use elf::{ElfBytes, endian::LittleEndian};
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
    #[error(transparent)]
    InvalidElf(#[from] elf::ParseError),
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
/// and map it writable at `page_addr` for relocation patching.
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

    // Map the private frame writable for relocation patching.
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

    // Read a bounded header window: Elf64_Ehdr (64 bytes) tells us where the
    // program-header and section-header tables are. We then read enough bytes to
    // cover [0, e_shoff + e_shentsize * e_shnum) in one shot, capped at 64 KiB.
    // If the tables extend beyond 64 KiB, fail with MissingSegments to keep the
    // kernel heap bounded (binary layout is unexpected).
    const HEADER_CAP: usize = 64 * 1024;

    // Read the fixed-size ELF header first.
    let ehdr_bytes = read_file_range(path, 0, 64)?;
    if ehdr_bytes.len() < 64 {
        return Err(ElfLoadError::MissingSegments);
    }

    // Minimal parse just to read table offsets/sizes from the header.
    let ehdr_elf: ElfBytes<'_, LittleEndian> = ElfBytes::minimal_parse(&ehdr_bytes)?;
    let ehdr = ehdr_elf.ehdr;

    if ehdr.e_machine != elf::abi::EM_X86_64 {
        return Err(ElfLoadError::UnsupportedArchitecture);
    }
    if ehdr.e_entry == 0 {
        return Err(ElfLoadError::NoEntryPoint);
    }

    // Compute the end of the section header table.
    let sh_end = (ehdr.e_shoff as usize)
        .saturating_add((ehdr.e_shentsize as usize).saturating_mul(ehdr.e_shnum as usize));

    if sh_end > HEADER_CAP {
        return Err(ElfLoadError::MissingSegments);
    }

    // Read [0, sh_end) -- covers ehdr, phdrs, and shdrs.
    let header_buf = read_file_range(path, 0, sh_end as u64)?;
    let elf_file: ElfBytes<'_, LittleEndian> = ElfBytes::minimal_parse(&header_buf)?;

    let header = elf_file.ehdr;

    let load_base = match header.e_type {
        elf::abi::ET_EXEC => VirtAddr::new(0),
        elf::abi::ET_DYN => VirtAddr::new(0x400000),
        _ => return Err(ElfLoadError::UnsupportedArchitecture),
    };

    let base_addr = load_base;
    let mut max_addr = 0u64;
    let mut memory_regions: Vec<Vma> = Vec::new();
    let mut tls_template: Option<TlsTemplate> = None;

    let program_headers = elf_file.segments().ok_or(ElfLoadError::MissingSegments)?;

    // Task 2.4: Build FileBacked + BSS VMAs for each PT_LOAD.
    for ph in program_headers.iter() {
        if ph.p_type == elf::abi::PT_LOAD {
            if ph.p_memsz == 0 {
                continue;
            }

            let vaddr = base_addr + ph.p_vaddr;
            let page_aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !0xfff);
            let vaddr_offset = vaddr.as_u64() - page_aligned_vaddr.as_u64();

            // Linker invariant: p_offset % p_align == p_vaddr % p_align, so
            // file_offset = p_offset - vaddr_offset is page-aligned. A linker that
            // violates this would produce incorrect page-cache lookups.
            debug_assert!(
                (ph.p_offset.wrapping_sub(vaddr_offset)) & 0xfff == 0,
                "ELF segment p_offset must share low bits with p_vaddr"
            );

            let file_offset = ph.p_offset - vaddr_offset; // page-aligned file byte offset

            let mut prot = VmaProt::empty();
            if ph.p_flags & elf::abi::PF_R != 0 {
                prot |= VmaProt::READ;
            }
            if ph.p_flags & elf::abi::PF_W != 0 {
                prot |= VmaProt::WRITE;
            }
            if ph.p_flags & elf::abi::PF_X != 0 {
                prot |= VmaProt::EXEC;
            }

            let writable_mapping = ph.p_flags & elf::abi::PF_W != 0;

            // Number of pages that contain file data (including the partial last page).
            let file_last_page_end = align_up(vaddr_offset + ph.p_filesz, 4096);
            let mem_last_page_end = align_up(vaddr_offset + ph.p_memsz, 4096);
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

            let segment_end = ph.p_vaddr + ph.p_memsz;
            max_addr = max_addr.max(segment_end + load_base.as_u64());
        } else if ph.p_type == elf::abi::PT_TLS {
            // Task 2.5: TLS template via targeted read_file_range.
            if ph.p_memsz == 0 {
                continue;
            }

            log!(
                "elf: TLS segment: filesz={} memsz={}",
                ph.p_filesz,
                ph.p_memsz
            );

            let init_data = if ph.p_filesz == 0 {
                Vec::new()
            } else {
                read_file_range(path, ph.p_offset, ph.p_filesz)?
            };

            tls_template = Some(TlsTemplate {
                init_data,
                mem_size: ph.p_memsz,
                align: ph.p_align.max(1),
            });
        }
    }

    // Task 2.7: Relocation loop using the page-cache helper.
    let write_flags =
        PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE;

    let section_headers = elf_file.section_headers().unwrap();
    let mut reloc_pages: BTreeSet<VirtAddr> = BTreeSet::new();

    for sh in section_headers.iter() {
        match sh.sh_type {
            elf::abi::SHT_RELA => {
                // Read relocation section data via targeted read_file_range.
                let rela_bytes = read_file_range(path, sh.sh_offset, sh.sh_size)?;

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
            elf::abi::SHT_REL => {
                panic!("REL relocation unsupported");
            }
            elf::abi::SHT_INIT_ARRAY => {
                println!("INIT ARRAY FOUND: {:?}", sh);
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

    let actual_entry = VirtAddr::new(load_base.as_u64() + header.e_entry);

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
