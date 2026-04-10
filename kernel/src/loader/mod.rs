use alloc::vec::Vec;
use elf::{ElfBytes, endian::LittleEndian};
use thiserror::Error;
use x86_64::{VirtAddr, align_up, structures::paging::PageTableFlags};

use crate::{
    log,
    memory::mapper::MemoryManager,
    println,
    thread::{MemoryRegion, MemoryRegionType},
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
    pub memory_regions: Vec<MemoryRegion>,
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

/// Data must be in kernel space
///
/// This function must be called using the process page.
pub fn load_elf(
    data: &[u8],
    memory_manager: &mut MemoryManager,
) -> Result<LoadedInfo, ElfLoadError> {
    let elf_file: ElfBytes<'_, LittleEndian> = ElfBytes::minimal_parse(data)?;

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

    // let common_data = elf_file.find_common_data()?;

    for header in program_headers.iter() {
        if header.p_type == elf::abi::PT_LOAD {
            /*
            println!(
                "ELF: Found PT_LOAD segment: vaddr=0x{:x}, filesz={}, memsz={}, flags=0x{:x}",
                header.p_vaddr, header.p_filesz, header.p_memsz, header.p_flags
            ); */

            let vaddr = base_addr + header.p_vaddr;
            let mem_size = header.p_memsz;
            let file_size = header.p_filesz;

            /*
            println!(
                "ELF: Loading segment at 0x{:x}, file_size: {}, mem_size: {}",
                vaddr.as_u64(),
                file_size,
                mem_size
            );
            */

            // todo: verify segments are valid

            // Align to page boundaries for mapping
            let page_aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !0xfff);
            let vaddr_offset = vaddr.as_u64() - page_aligned_vaddr.as_u64();
            let aligned_size = (mem_size + vaddr_offset + 0xfff) & !0xfff;

            // Writeable flags first, because we need to write data and relocations
            let flags = PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE;

            /*
            println!(
                "ELF: Mapping aligned region: 0x{:x}-0x{:x} (size: 0x{:x})",
                page_aligned_vaddr.as_u64(),
                page_aligned_vaddr.as_u64() + aligned_size,
                aligned_size
            );
            */

            // Map memory for the entire segment (including BSS if mem_size > file_size)
            let _range = memory_manager
                .map_memory(page_aligned_vaddr, aligned_size, flags)
                .map_err(|e| {
                    println!("ELF: Mapping failed: {:?}", e);
                    ElfLoadError::MappingFailed
                })?;

            // Copy file data into memory
            if file_size > 0 {
                let segment_data = elf_file.segment_data(&header)?;
                memory_manager.copy_to_user(vaddr, segment_data);
            }

            // Zero out BSS section (mem_size > file_size)
            if mem_size > file_size {
                let bss_start = vaddr + file_size;
                memory_manager.zero_user(bss_start, (mem_size - file_size) as usize);
            }

            let region = MemoryRegion {
                start: page_aligned_vaddr,
                size: aligned_size,
                flags,
                region_type: if header.p_flags & elf::abi::PF_X != 0 {
                    MemoryRegionType::Code
                } else {
                    MemoryRegionType::Data
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

    let section_headers = elf_file.section_headers().unwrap();

    for section_header in section_headers.iter() {
        match section_header.sh_type {
            elf::abi::SHT_RELA => {
                let rela_entries = elf_file.section_data_as_relas(&section_header)?;

                for rela in rela_entries {
                    let reloc_addr = base_addr + rela.r_offset;
                    let reloc_type = rela.r_type;
                    //println!("Relocation rela R_X86_64_RELATIVE: {}", rela.r_offset);

                    match reloc_type {
                        // R_X86_64_RELATIVE - Most common for PIC static executables
                        8 => {
                            let value: u64 = base_addr.as_u64() + rela.r_addend as u64;
                            memory_manager.write_val_to_user(reloc_addr, value);
                        }

                        _ => {
                            println!("Unsupported relocation type: {}", reloc_type);
                            // For now, continue - many relocations might not be needed
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

    // TODO: same for .fini_array destructors

    // Set proper page flags.
    for header in program_headers.iter() {
        if header.p_type == elf::abi::PT_LOAD {
            let vaddr = base_addr + header.p_vaddr;
            let mem_size = header.p_memsz;

            // Align to page boundaries for mapping
            let page_aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !0xfff);
            let vaddr_offset = vaddr.as_u64() - page_aligned_vaddr.as_u64();
            let aligned_size = (mem_size + vaddr_offset + 0xfff) & !0xfff;

            let mut flags = PageTableFlags::USER_ACCESSIBLE;

            if header.p_flags & elf::abi::PF_W != 0 {
                flags |= PageTableFlags::WRITABLE;
            }

            if header.p_flags & elf::abi::PF_X == 0 {
                flags |= PageTableFlags::NO_EXECUTE;
            }

            // If this is a code segment, remove write permissions after loading
            memory_manager
                .change_flags(page_aligned_vaddr, aligned_size, flags)
                .map_err(|e| {
                    println!("ELF: Failed to update flags: {:?}", e);
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
