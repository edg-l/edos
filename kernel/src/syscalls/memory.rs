use alloc::format;

use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{Mapper, Page, PageTableFlags, Size4KiB},
};

use crate::{
    log,
    memory::{
        mapper::memory_mapper,
        pat,
        vma::{Vma, VmaBacking, VmaFlags, VmaProt},
    },
    println,
    syscalls::Errno,
    thread::scheduler::sched,
};

// Protection flags (match Linux)
const PROT_READ: u32 = 0x1;
const PROT_WRITE: u32 = 0x2;
const PROT_EXEC: u32 = 0x4;

// Mapping flags
pub const MAP_SHARED: u32 = 0x01;
pub const MAP_PRIVATE: u32 = 0x02;
pub const MAP_FIXED: u32 = 0x10;
pub const MAP_ANONYMOUS: u32 = 0x20;
pub const MAP_PHYSICAL: u32 = 0x40;
pub const MAP_WRITE_COMBINING: u32 = 0x80;

// msync flags
pub const MS_ASYNC: u32 = 0x1;
pub const MS_SYNC: u32 = 0x2;
pub const MS_INVALIDATE: u32 = 0x4;

/// `r8` is overloaded: physical address for MAP_PHYSICAL, fd for file-backed mappings.
/// `r9` is the file offset (used only for file-backed mappings).
pub fn sys_mmap(addr: u64, length: u64, prot: u32, flags: u32, r8: u64, r9: u64) -> u64 {
    let phys_addr = r8; // alias for MAP_PHYSICAL path
    let _file_offset = r9; // used in Phase B for file-backed path
    let prot_str = match (
        prot & PROT_READ != 0,
        prot & PROT_WRITE != 0,
        prot & PROT_EXEC != 0,
    ) {
        (true, true, true) => "rwx",
        (true, true, false) => "rw-",
        (true, false, true) => "r-x",
        (true, false, false) => "r--",
        (false, true, false) => "-w-",
        (false, false, true) => "--x",
        _ => "---",
    };
    let kind = if flags & MAP_PHYSICAL != 0 {
        format!("physical @ {phys_addr:#x}")
    } else if flags & MAP_ANONYMOUS != 0 {
        "anonymous".into()
    } else {
        format!("file-backed fd={r8} off={r9:#x}")
    };
    log!(
        "mmap: addr={addr:#x} len={length:#x} ({} KiB) prot={prot_str} {kind}",
        length / 1024
    );
    let sched = sched();
    let info = sched.current_thread_info();

    info.lock().errno = Errno::Clear;

    if length == 0 {
        info.lock().errno = Errno::EINVAL;
        return !0u64; // -1 (EINVAL)
    }

    let is_physical = (flags & MAP_PHYSICAL) != 0;

    // Access VmaSet from UserThread
    let thread = match sched.current_thread() {
        Some(t) => t,
        None => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };
    let user_arc = match &thread.user {
        Some(u) => u.clone(),
        None => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    if is_physical {
        // Validate physical address alignment
        if phys_addr & 0xFFF != 0 {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }

        // Only allow mapping physical ranges that the kernel has explicitly registered
        // (e.g. VRAM). Prevents userspace from mapping arbitrary physical memory.
        if !crate::memory::is_physical_range_allowed(phys_addr, length) {
            info.lock().errno = Errno::EPERM;
            return !0u64;
        }

        let map_addr = if addr == 0 {
            let user_read = user_arc.read();
            let next_mmap_addr = info.lock().next_mmap_addr.clone();
            let vmas = user_read.vmas.lock();
            vmas.find_free_address(&next_mmap_addr, length)
        } else {
            VirtAddr::new(addr)
        };

        let mut phys_flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::NO_EXECUTE;
        if (flags & MAP_WRITE_COMBINING) != 0 {
            phys_flags |= pat::WRITE_COMBINING;
        }

        let page_count = (length + 0xFFF) / 4096;
        let memory_manager = info.lock().memory_manager.clone();
        let mut mm = memory_manager.lock();
        for i in 0..page_count {
            let virt = VirtAddr::new(map_addr.as_u64() + i * 4096);
            let phys = PhysAddr::new(phys_addr + i * 4096);
            if mm
                .map_address(virt, phys, phys_flags & !PageTableFlags::PRESENT)
                .is_err()
            {
                // Rollback already-mapped pages
                for j in 0..i {
                    let rollback_virt = VirtAddr::new(map_addr.as_u64() + j * 4096);
                    let page: Page<Size4KiB> = Page::containing_address(rollback_virt);
                    if let Ok((_, flush)) = mm.mapper.unmap(page) {
                        flush.flush();
                    }
                }
                log!("Error mapping physical page");
                drop(mm);
                info.lock().errno = Errno::ENOMEM;
                return !0u64;
            }
        }
        drop(mm);

        let mut vma_prot = VmaProt::empty();
        if prot & PROT_READ != 0 {
            vma_prot |= VmaProt::READ;
        }
        if prot & PROT_WRITE != 0 {
            vma_prot |= VmaProt::WRITE;
        }
        if prot & PROT_EXEC != 0 {
            vma_prot |= VmaProt::EXEC;
        }

        user_arc.read().vmas.lock().insert(Vma {
            start: map_addr,
            end: map_addr + length,
            prot: vma_prot,
            flags: VmaFlags::PRIVATE,
            backing: VmaBacking::Physical {
                phys_base: phys_addr,
            },
        });

        log!("mmap: mapped physical at {map_addr:p}");
        map_addr.as_u64()
    } else if (flags & MAP_ANONYMOUS) != 0 || r8 == u64::MAX {
        // Anonymous mapping (MAP_ANONYMOUS set, or fd == -1).
        if (flags & MAP_PRIVATE) == 0 && (flags & MAP_SHARED) == 0 {
            println!("mmap: anonymous mapping must have MAP_PRIVATE or MAP_SHARED");
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }

        let map_addr = if addr == 0 {
            let user_read = user_arc.read();
            let next_mmap_addr = info.lock().next_mmap_addr.clone();
            let vmas = user_read.vmas.lock();
            vmas.find_free_address(&next_mmap_addr, length)
        } else {
            VirtAddr::new(addr)
        };

        let mut vma_prot = VmaProt::empty();
        if prot & PROT_READ != 0 {
            vma_prot |= VmaProt::READ;
        }
        if prot & PROT_WRITE != 0 {
            vma_prot |= VmaProt::WRITE;
        }
        if prot & PROT_EXEC != 0 {
            vma_prot |= VmaProt::EXEC;
        }

        // Lazy allocation - just record the VMA, don't allocate frames
        user_arc.read().vmas.lock().insert(Vma {
            start: map_addr,
            end: map_addr + length,
            prot: vma_prot,
            flags: VmaFlags::PRIVATE | VmaFlags::LAZY,
            backing: VmaBacking::Anonymous,
        });

        log!("mmap: lazy mapped at {map_addr:p}");
        map_addr.as_u64()
    } else {
        // File-backed mapping: not yet implemented (Phase B).
        log!("mmap: file-backed mapping not yet implemented (fd={r8} off={r9:#x})");
        info.lock().errno = Errno::EINVAL;
        !0u64
    }
}

pub fn sys_munmap(addr: u64, length: u64) -> i32 {
    log!("Unmapping {addr} {length}");
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if length == 0 {
        info.lock().errno = Errno::EINVAL;
        return -1; // EINVAL
    }

    let map_addr = VirtAddr::new(addr);

    let thread = match sched.current_thread() {
        Some(t) => t,
        None => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };
    let user_arc = match &thread.user {
        Some(u) => u.clone(),
        None => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    // Remove the VMA - short lock
    let vma = user_arc.read().vmas.lock().remove(&map_addr);
    if let Some(vma) = vma {
        if vma.size() == length {
            match &vma.backing {
                VmaBacking::Anonymous => {
                    let memory_manager = info.lock().memory_manager.clone();
                    let page_count = (length + 0xFFF) / 4096;
                    let mut frames = alloc::vec::Vec::new();
                    {
                        let mut mm = memory_manager.lock();
                        for i in 0..page_count {
                            let virt = VirtAddr::new(map_addr.as_u64() + i * 4096);
                            let page: Page<Size4KiB> = Page::containing_address(virt);
                            // Only unmap pages that are actually present (lazy pages may not be)
                            if let Ok(phys) = mm.mapper.translate_page(page) {
                                if let Ok((_, flush)) = mm.mapper.unmap(page) {
                                    flush.ignore(); // Will do TLB shootdown below
                                    frames.push(phys);
                                }
                            }
                        }
                    }
                    if !frames.is_empty() && crate::memory::tlb::shootdown_needed() {
                        crate::memory::tlb::tlb_shootdown(map_addr, page_count);
                    }
                    let mut fa = crate::memory::frame_allocator::frame_allocator();
                    for frame in frames {
                        unsafe { fa.deallocate_frame(frame) };
                    }
                    log!("Unmap success");
                    0
                }
                VmaBacking::Physical { .. } => {
                    let page_count = (length + 0xFFF) / 4096;
                    {
                        let mut mapper = memory_mapper();
                        for i in 0..page_count {
                            let virt = VirtAddr::new(addr + i * 4096);
                            let page: Page<Size4KiB> = Page::containing_address(virt);
                            if let Ok((_, flush)) = mapper.mapper.unmap(page) {
                                flush.flush();
                            }
                        }
                    }
                    if crate::memory::tlb::shootdown_needed() {
                        crate::memory::tlb::tlb_shootdown(map_addr, page_count);
                    }
                    log!("Unmap physical success");
                    0
                }
                VmaBacking::SharedMemory { .. } => {
                    // Shared memory should be unmapped via sys_shm_unmap
                    user_arc.read().vmas.lock().insert(vma);
                    info.lock().errno = Errno::EINVAL;
                    -1
                }
                VmaBacking::ElfSegment { .. } | VmaBacking::Tls | VmaBacking::Stack => {
                    // These are kernel-managed; put back and return error
                    user_arc.read().vmas.lock().insert(vma);
                    info.lock().errno = Errno::EINVAL;
                    -1
                }
                VmaBacking::FileBacked { .. } => {
                    // Phase B will implement proper unmap with frame/pin cleanup.
                    // For Phase A no file-backed VMAs are created, so this is unreachable.
                    user_arc.read().vmas.lock().insert(vma);
                    info.lock().errno = Errno::EINVAL;
                    -1
                }
            }
        } else {
            log!("Unmap fail, partial");
            // Re-insert since we can't handle partial unmapping
            user_arc.read().vmas.lock().insert(vma);
            info.lock().errno = Errno::EINVAL;
            -1
        }
    } else {
        log!("Unmap fail, einval");
        info.lock().errno = Errno::EINVAL;
        -1
    }
}
