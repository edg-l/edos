use alloc::vec::Vec;
use elf::{ElfBytes, endian::LittleEndian};
use thiserror::Error;
use x86_64::{
    VirtAddr, align_up, instructions::interrupts::without_interrupts,
    structures::paging::PageTableFlags,
};

use crate::{
    memory::mapper::MemoryManager,
    println,
    thread::user::{MemoryRegion, MemoryRegionType},
};

#[derive(Debug, Clone)]
pub struct LoadedInfo {
    pub entry_point: VirtAddr,
    pub heap_break: u64,
    pub memory_regions: Vec<MemoryRegion>,
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
    without_interrupts(|| {
        let elf_file: ElfBytes<'_, LittleEndian> = ElfBytes::minimal_parse(data)?;

        let header = elf_file.ehdr;
        println!(
            "ELF: Architecture: 0x{:x}, Type: 0x{:x}",
            header.e_machine, header.e_type
        );

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
                println!("ELF: Loading ET_EXEC (fixed address) executable");
                VirtAddr::new(0)
            }
            elf::abi::ET_DYN => {
                // Position Independent Executable - choose load base
                let base = VirtAddr::new(0x400000);
                println!(
                    "ELF: Loading ET_DYN (PIC) executable at base 0x{:x}",
                    base.as_u64()
                );
                base
            }
            _ => return Err(ElfLoadError::UnsupportedArchitecture),
        };

        let mut max_addr = 0u64;

        let mut memory_regions = Vec::new();
        let base_addr = load_base;

        // let common_data = elf_file.find_common_data()?;

        for header in program_headers.iter() {
            if header.p_type == elf::abi::PT_LOAD {
                println!(
                    "ELF: Found PT_LOAD segment: vaddr=0x{:x}, filesz={}, memsz={}, flags=0x{:x}",
                    header.p_vaddr, header.p_filesz, header.p_memsz, header.p_flags
                );

                let vaddr = base_addr + header.p_vaddr;
                let mem_size = header.p_memsz;
                let file_size = header.p_filesz;

                println!(
                    "ELF: Loading segment at 0x{:x}, file_size: {}, mem_size: {}",
                    vaddr.as_u64(),
                    file_size,
                    mem_size
                );

                // todo: verify segments are valid

                // Align to page boundaries for mapping
                let page_aligned_vaddr = VirtAddr::new(vaddr.as_u64() & !0xfff);
                let vaddr_offset = vaddr.as_u64() - page_aligned_vaddr.as_u64();
                let aligned_size = (mem_size + vaddr_offset + 0xfff) & !0xfff;

                // Writeable flags first, because we need to write data and relocations
                let flags = PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE;

                println!(
                    "ELF: Mapping aligned region: 0x{:x}-0x{:x} (size: 0x{:x})",
                    page_aligned_vaddr.as_u64(),
                    page_aligned_vaddr.as_u64() + aligned_size,
                    aligned_size
                );

                // Map memory for the entire segment (including BSS if mem_size > file_size)
                let range = memory_manager
                    .map_memory(page_aligned_vaddr, aligned_size, flags)
                    .map_err(|e| {
                        println!("ELF: Mapping failed: {:?}", e);
                        ElfLoadError::MappingFailed
                    })?;

                println!("Mapped range {range:?}");

                // Copy file data into memory
                if file_size > 0 {
                    let segment_data = elf_file.segment_data(&header)?;

                    println!(
                        "ELF: Copying {} bytes of segment data, segment_data {:p} {}",
                        segment_data.len(),
                        segment_data.as_ptr(),
                        segment_data.as_ptr().align_offset(align_of::<u8>())
                    );

                    // copy data
                    unsafe {
                        let dest = vaddr.as_u64() as *mut u8;
                        core::ptr::copy_nonoverlapping(
                            segment_data.as_ptr(),
                            dest,
                            segment_data.len(),
                        );
                    }
                }

                // Zero out BSS section (mem_size > file_size)
                if mem_size > file_size {
                    let bss_start = vaddr + file_size;
                    let bss_size = mem_size - file_size;
                    println!(
                        "ELF: Zeroing BSS section: 0x{:x}-0x{:x} (size: {})",
                        bss_start.as_u64(),
                        bss_start.as_u64() + bss_size,
                        bss_size
                    );
                    unsafe {
                        let dest = bss_start.as_u64() as *mut u8;
                        core::ptr::write_bytes(dest, 0, bss_size as usize);
                    }
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
                                let value = base_addr.as_u64() + rela.r_addend as u64;

                                unsafe {
                                    *(reloc_addr.as_u64() as *mut u64) = value;
                                }
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
            println!("ELF: No PT_LOAD segments found, using default heap start");
            max_addr = 0x10000000;
        }

        let actual_entry = VirtAddr::new(load_base.as_u64() + header.e_entry);

        println!("ELF: header.e_entry = 0x{:x}", header.e_entry);
        println!("ELF: load_base = {:p}", load_base.as_u64() as *const u8);
        println!("ELF: calculated entry = {actual_entry:p}");

        Ok(LoadedInfo {
            entry_point: actual_entry,
            heap_break: align_up(max_addr, 4096) + 0x10000,
            memory_regions,
        })
    })
}
