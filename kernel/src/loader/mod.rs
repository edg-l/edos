use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use elf::{ElfBytes, endian::LittleEndian};
use thiserror::Error;
use x86_64::{VirtAddr, align_up, structures::paging::PageTableFlags};

use crate::{
    log,
    memory::{
        frame_allocator::frame_allocator,
        mapper::MemoryManager,
        vma::{Vma, VmaBacking, VmaFlags, VmaProt},
    },
    println,
};

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
}

/// Eagerly fault a single page within an ELF segment VMA.
/// Allocates a frame, fills it from ELF data (with proper BSS zeroing),
/// and maps it into the address space.
fn eager_fault_elf_page(
    memory_manager: &mut MemoryManager,
    elf_data: &[u8],
    page_addr: VirtAddr,
    vma_start: VirtAddr,
    file_offset: u64,
    file_size: u64,
    vaddr_offset: u64,
    flags: PageTableFlags,
) -> Result<(), ElfLoadError> {
    use x86_64::structures::paging::FrameAllocator;

    let frame = frame_allocator()
        .allocate_frame()
        .ok_or(ElfLoadError::MappingFailed)?;

    let phys_offset = crate::boot::boot_info().physical_memory_offset;
    let frame_ptr = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
    // Zero the frame first
    unsafe { core::ptr::write_bytes(frame_ptr, 0, 4096) };

    // Compute file data range for this page
    let page_off_in_vma = page_addr.as_u64() - vma_start.as_u64();
    // Segment byte offset for start of this page
    let seg_start = page_off_in_vma.saturating_sub(vaddr_offset);
    // Segment byte offset for end of this page
    let seg_end = (page_off_in_vma + 4096).saturating_sub(vaddr_offset);

    // Clamp to file_size to get the file data portion
    let copy_start = seg_start.min(file_size);
    let copy_end = seg_end.min(file_size);

    if copy_end > copy_start {
        let elf_src_offset = (file_offset + copy_start) as usize;
        let elf_src_end = (file_offset + copy_end) as usize;
        if elf_src_end <= elf_data.len() {
            // Destination offset within the page: if the segment starts mid-page,
            // there is vaddr_offset bytes of padding before the data in the first page.
            let dst_offset = if page_off_in_vma < vaddr_offset {
                (vaddr_offset - page_off_in_vma) as usize
            } else {
                0usize
            };
            let copy_len = (copy_end - copy_start) as usize;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    elf_data[elf_src_offset..].as_ptr(),
                    frame_ptr.add(dst_offset),
                    copy_len,
                );
            }
        }
    }

    memory_manager
        .map_address(page_addr, frame.start_address(), flags)
        .map_err(|_| ElfLoadError::MappingFailed)?;

    Ok(())
}

/// Data must be in kernel space
///
/// This function must be called using the process page.
pub fn load_elf(
    data: Arc<Vec<u8>>,
    memory_manager: &mut MemoryManager,
) -> Result<LoadedInfo, ElfLoadError> {
    let elf_file: ElfBytes<'_, LittleEndian> = ElfBytes::minimal_parse(&data)?;

    let header = elf_file.ehdr;

    if header.e_machine != elf::abi::EM_X86_64 {
        return Err(ElfLoadError::UnsupportedArchitecture);
    }

    // Get program headers
    let program_headers = elf_file.segments().ok_or(ElfLoadError::MissingSegments)?;

    if header.e_entry == 0 {
        return Err(ElfLoadError::NoEntryPoint);
    }

    let load_base = match header.e_type {
        elf::abi::ET_EXEC => {
            // Fixed address executable - use addresses from ELF
            VirtAddr::new(0)
        }
        elf::abi::ET_DYN => {
            // Position Independent Executable - choose load base
            VirtAddr::new(0x400000)
        }
        _ => return Err(ElfLoadError::UnsupportedArchitecture),
    };

    let mut max_addr = 0u64;

    let mut memory_regions = Vec::new();
    let mut tls_template: Option<TlsTemplate> = None;
    let base_addr = load_base;

    for header in program_headers.iter() {
        if header.p_type == elf::abi::PT_LOAD {
            let vaddr = base_addr + header.p_vaddr;
            let mem_size = header.p_memsz;
            let file_size = header.p_filesz;

            // Align to page boundaries for mapping
            let page_aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !0xfff);
            let vaddr_offset = vaddr.as_u64() - page_aligned_vaddr.as_u64();
            let aligned_size = (mem_size + vaddr_offset + 0xfff) & !0xfff;

            let mut prot = VmaProt::empty();
            if header.p_flags & elf::abi::PF_R != 0 {
                prot |= VmaProt::READ;
            }
            if header.p_flags & elf::abi::PF_W != 0 {
                prot |= VmaProt::WRITE;
            }
            if header.p_flags & elf::abi::PF_X != 0 {
                prot |= VmaProt::EXEC;
            }

            // Create a lazy VMA backed by ELF data -- no pages mapped yet.
            let region = Vma {
                start: page_aligned_vaddr,
                end: page_aligned_vaddr + aligned_size,
                prot,
                flags: VmaFlags::PRIVATE | VmaFlags::LAZY,
                backing: VmaBacking::ElfSegment {
                    elf_data: data.clone(),
                    file_offset: header.p_offset,
                    file_size,
                    vaddr_offset,
                },
            };

            memory_regions.push(region);

            let segment_end = header.p_vaddr + header.p_memsz;
            max_addr = max_addr.max(segment_end + load_base.as_u64());
        } else if header.p_type == elf::abi::PT_TLS {
            if header.p_memsz == 0 {
                continue;
            }

            log!(
                "elf: TLS segment: filesz={} memsz={}",
                header.p_filesz,
                header.p_memsz
            );

            let align = header.p_align.max(1);
            let init_data = if header.p_filesz == 0 {
                Vec::new()
            } else {
                elf_file.segment_data(&header)?.to_vec()
            };

            tls_template = Some(TlsTemplate {
                init_data,
                mem_size: header.p_memsz,
                align,
            });
        }
    }

    // Process relocations: eagerly fault target pages (writable), apply relocations,
    // then fix permissions on those pages. Remaining pages are faulted lazily with
    // correct VMA permissions.
    let section_headers = elf_file.section_headers().unwrap();
    let mut reloc_pages: BTreeSet<VirtAddr> = BTreeSet::new();

    for section_header in section_headers.iter() {
        match section_header.sh_type {
            elf::abi::SHT_RELA => {
                let rela_entries = elf_file.section_data_as_relas(&section_header)?;

                for rela in rela_entries {
                    let reloc_addr = base_addr + rela.r_offset;
                    let reloc_type = rela.r_type;

                    match reloc_type {
                        // R_X86_64_RELATIVE - most common for PIC static executables
                        8 => {
                            let page_addr = VirtAddr::new(reloc_addr.as_u64() & !0xfff);

                            // Eagerly fault this page if not already faulted
                            if reloc_pages.insert(page_addr) {
                                // Find which VMA this relocation belongs to
                                if let Some(region) =
                                    memory_regions.iter().find(|r| r.contains(page_addr))
                                {
                                    if let VmaBacking::ElfSegment {
                                        elf_data: ref ed,
                                        file_offset,
                                        file_size,
                                        vaddr_offset,
                                    } = region.backing
                                    {
                                        eager_fault_elf_page(
                                            memory_manager,
                                            ed,
                                            page_addr,
                                            region.start,
                                            file_offset,
                                            file_size,
                                            vaddr_offset,
                                            PageTableFlags::PRESENT
                                                | PageTableFlags::USER_ACCESSIBLE
                                                | PageTableFlags::WRITABLE,
                                        )?;
                                    }
                                }
                            }

                            let value: u64 = base_addr.as_u64() + rela.r_addend as u64;
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
                println!("INIT ARRAY FOUND: {:?}", section_header);
            }
            _ => {}
        }
    }

    // Set correct permissions on eagerly-faulted relocation pages.
    // These were mapped WRITABLE for relocation patching; now set to final perms.
    for &page_addr in &reloc_pages {
        if let Some(region) = memory_regions.iter().find(|r| r.contains(page_addr)) {
            let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if region.prot.contains(VmaProt::WRITE) {
                flags |= PageTableFlags::WRITABLE;
            }
            if !region.prot.contains(VmaProt::EXEC) {
                flags |= PageTableFlags::NO_EXECUTE;
            }
            memory_manager
                .change_flags(page_addr, 4096, flags)
                .map_err(|e| {
                    println!("ELF: Failed to update flags on reloc page: {:?}", e);
                    ElfLoadError::MappingFailed
                })?;
        }
    }

    if max_addr == 0 {
        // No loadable segments found, fall back to constant
        max_addr = 0x10000000;
    }

    let actual_entry = VirtAddr::new(load_base.as_u64() + header.e_entry);

    log!(
        "elf: loaded at {:#x}, entry={:#x}",
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
