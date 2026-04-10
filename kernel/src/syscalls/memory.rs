use alloc::{collections::btree_map::BTreeMap, format, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{Mapper, Page, PageTableFlags, Size4KiB},
};

use crate::{
    log,
    memory::{mapper::memory_mapper, pat},
    println,
    syscalls::Errno,
    thread::{MappingType, MemoryMapping, UserThreadInfo, mutex::BlockingMutex, scheduler::sched},
};

// Protection flags (match Linux)
const PROT_READ: u32 = 0x1;
const PROT_WRITE: u32 = 0x2;
const PROT_EXEC: u32 = 0x4;

// Mapping flags
const MAP_ANONYMOUS: u32 = 0x20;
const MAP_PRIVATE: u32 = 0x02;
const MAP_PHYSICAL: u32 = 0x40;
const MAP_WRITE_COMBINING: u32 = 0x80;

pub fn sys_mmap(addr: u64, length: u64, prot: u32, flags: u32, phys_addr: u64) -> u64 {
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
        "file-backed".into()
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

        // Physical mapping: map MMIO/VRAM pages directly without allocating frames
        let map_addr = if addr == 0 {
            let guard = info.lock();
            find_free_virtual_address_atomic(&guard.memory_mappings, &guard.next_mmap_addr, length)
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

        info.lock().memory_mappings.lock().insert(
            map_addr,
            MemoryMapping {
                size: length,
                flags: phys_flags,
                mapping_type: MappingType::Physical(phys_addr),
            },
        );

        log!("mmap: mapped physical at {map_addr:p}");
        map_addr.as_u64()
    } else {
        // Only support anonymous private mappings otherwise
        if (flags & MAP_ANONYMOUS) == 0 || (flags & MAP_PRIVATE) == 0 {
            println!("Unsupported mapping type");
            info.lock().errno = Errno::EINVAL;
            return !0u64; // -1 (EINVAL)
        }

        let map_addr = if addr == 0 {
            let guard = info.lock();
            find_free_virtual_address_atomic(&guard.memory_mappings, &guard.next_mmap_addr, length)
        } else {
            VirtAddr::new(addr)
        };

        // Convert protection flags
        let mut page_flags = PageTableFlags::USER_ACCESSIBLE;
        if prot & PROT_WRITE != 0 {
            page_flags |= PageTableFlags::WRITABLE;
        }
        if prot & PROT_EXEC == 0 {
            page_flags |= PageTableFlags::NO_EXECUTE;
        }

        // Map the memory
        let memory_manager = info.lock().memory_manager.clone();
        if memory_manager
            .lock()
            .map_memory(map_addr, length, page_flags)
            .is_ok()
        {
            info.lock().memory_mappings.lock().insert(
                map_addr,
                MemoryMapping {
                    size: length,
                    flags: page_flags,
                    mapping_type: MappingType::Anonymous,
                },
            );

            log!("mmap: mapped at {map_addr:p}");

            map_addr.as_u64()
        } else {
            log!("Error mapping");
            info.lock().errno = Errno::ENOMEM;
            !0u64 // -1 (ENOMEM)
        }
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

    // Check if this is a valid mapping - short lock
    let mapping = info.lock().memory_mappings.lock().remove(&map_addr);
    if let Some(mapping) = mapping {
        if mapping.size == length {
            match mapping.mapping_type {
                MappingType::Anonymous => {
                    let memory_manager = info.lock().memory_manager.clone();
                    // Unmap PTEs and collect frames without freeing them.
                    // Free frames only AFTER the TLB shootdown to prevent
                    // use-after-free of physical memory via stale TLB entries.
                    let frames = memory_manager
                        .lock()
                        .unmap_memory_deferred(map_addr, length);
                    match frames {
                        Ok(frames) => {
                            if crate::memory::tlb::shootdown_needed() {
                                crate::memory::tlb::tlb_shootdown(
                                    map_addr,
                                    (length + 0xFFF) / 4096,
                                );
                            }
                            // Now safe to free the frames.
                            let mut fa = crate::memory::frame_allocator::frame_allocator();
                            for frame in frames {
                                unsafe { fa.deallocate_frame(frame) };
                            }
                            log!("Unmap success");
                            0
                        }
                        Err(_) => {
                            log!("Unmap fault");
                            info.lock().errno = Errno::EFAULT;
                            -1
                        }
                    }
                }
                MappingType::Physical(_phys_base) => {
                    // Physical (MMIO/VRAM): unmap pages without deallocating frames
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
                    } // mapper dropped here, interrupts re-enabled
                    if crate::memory::tlb::shootdown_needed() {
                        crate::memory::tlb::tlb_shootdown(map_addr, page_count);
                    }
                    log!("Unmap physical success");
                    0
                }
                MappingType::Shared(_) => {
                    // Shared memory should be unmapped via sys_shm_unmap
                    info.lock().memory_mappings.lock().insert(map_addr, mapping);
                    info.lock().errno = Errno::EINVAL;
                    -1
                }
            }
        } else {
            log!("Unmap fail, partial");
            // Re-insert the mapping since we couldn't handle partial unmapping
            info.lock().memory_mappings.lock().insert(map_addr, mapping);
            info.lock().errno = Errno::EINVAL;
            -1 // EINVAL - partial unmapping not supported yet
        }
    } else {
        log!("Unmap fail, einval");
        info.lock().errno = Errno::EINVAL;
        -1 // EINVAL - not a valid mapping
    }
}

/// Find free virtual address with atomic next_mmap_addr and locked memory_mappings
pub fn find_free_virtual_address_atomic(
    memory_mappings: &Arc<BlockingMutex<BTreeMap<VirtAddr, MemoryMapping>>>,
    next_mmap_addr: &Arc<AtomicU64>,
    length: u64,
) -> VirtAddr {
    let aligned_length = (length + 0xfff) & !0xfff;

    loop {
        // Atomically allocate address range
        let candidate_u64 = next_mmap_addr.fetch_add(aligned_length, Ordering::Relaxed);
        let candidate = VirtAddr::new(candidate_u64);
        let end_addr = candidate + aligned_length;

        // Check if this range overlaps with existing mappings - short lock
        let mappings = memory_mappings.lock();
        let mut overlaps = false;
        for (&mapping_start, mapping) in mappings.iter() {
            let mapping_end = mapping_start + mapping.size;
            if !(end_addr <= mapping_start || candidate >= mapping_end) {
                overlaps = true;
                break;
            }
        }
        drop(mappings); // Release lock immediately

        if !overlaps {
            return candidate;
        }

        // Address collided, atomic counter already advanced so try again
    }
}

/// Legacy function for compatibility - now uses atomic version
pub fn find_free_virtual_address(thread: &mut UserThreadInfo, length: u64) -> VirtAddr {
    find_free_virtual_address_atomic(&thread.memory_mappings, &thread.next_mmap_addr, length)
}
