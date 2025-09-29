use x86_64::{VirtAddr, structures::paging::PageTableFlags};

use crate::{
    log, println,
    syscalls::Errno,
    thread::{MappingType, MemoryMapping, UserThreadInfo, scheduler::sched},
};

// Protection flags (match Linux)
#[expect(unused)]
const PROT_READ: u32 = 0x1;
const PROT_WRITE: u32 = 0x2;
const PROT_EXEC: u32 = 0x4;

// Mapping flags
const MAP_ANONYMOUS: u32 = 0x20;
const MAP_PRIVATE: u32 = 0x02;

pub fn sys_mmap(addr: u64, length: u64, prot: u32, flags: u32) -> u64 {
    log!("MMap: {addr} {length} {prot} {flags}");
    let sched = sched();
    let info = sched.current_thread_info();

    info.lock().errno = Errno::Clear;

    if length == 0 {
        info.lock().errno = Errno::EINVAL;
        return !0u64; // -1 (EINVAL)
    }

    // Only support anonymous private mappings for now
    if (flags & MAP_ANONYMOUS) == 0 || (flags & MAP_PRIVATE) == 0 {
        println!("Unsupported mapping type");
        info.lock().errno = Errno::EINVAL;
        return !0u64; // -1 (EINVAL)
    }

    let map_addr = if addr == 0 {
        // Find free virtual address
        find_free_virtual_address(&mut info.lock(), length)
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
    if info
        .lock()
        .memory_manager
        .lock()
        .map_memory(map_addr, length, page_flags)
        .is_ok()
    {
        info.lock().memory_mappings.insert(
            map_addr,
            MemoryMapping {
                size: length,
                flags: page_flags,
                mapping_type: MappingType::Anonymous,
            },
        );

        log!("Returning {map_addr:p} {page_flags:?}");

        map_addr.as_u64()
    } else {
        log!("Error mapping");
        info.lock().errno = Errno::ENOMEM;
        !0u64 // -1 (ENOMEM)
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

    // Check if this is a valid mapping
    let mapping = info.lock().memory_mappings.remove(&map_addr);
    if let Some(mapping) = mapping {
        if mapping.size == length {
            // Unmap the memory
            if info
                .lock()
                .memory_manager
                .lock()
                .unmap_memory(map_addr, length)
                .is_ok()
            {
                log!("Unmap success");
                0 // Success
            } else {
                log!("Unmap fault");
                info.lock().errno = Errno::EFAULT;
                -1 // EFAULT
            }
        } else {
               log!("Unmap fail, partial");
            // Re-insert the mapping since we couldn't handle partial unmapping
            info.lock().memory_mappings.insert(map_addr, mapping);
            info.lock().errno = Errno::EINVAL;
            -1 // EINVAL - partial unmapping not supported yet
        }
    } else {
         log!("Unmap fail, einval");
        info.lock().errno = Errno::EINVAL;
        -1 // EINVAL - not a valid mapping
    }
}

fn find_free_virtual_address(thread: &mut UserThreadInfo, length: u64) -> VirtAddr {
    let aligned_length = (length + 0xfff) & !0xfff;

    loop {
        let candidate = thread.next_mmap_addr;
        let end_addr = candidate + aligned_length;

        // Check if this range overlaps with existing mappings
        let mut overlaps = false;
        for (&mapping_start, mapping) in &thread.memory_mappings {
            let mapping_end = mapping_start + mapping.size;
            if !(end_addr <= mapping_start || candidate >= mapping_end) {
                overlaps = true;
                break;
            }
        }

        if !overlaps {
            thread.next_mmap_addr = end_addr;
            return candidate;
        }

        // Move to next potential address
        thread.next_mmap_addr += 0x10000; // 64KB increment
    }
}
