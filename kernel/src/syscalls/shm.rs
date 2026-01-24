//! Shared memory syscalls
//!
//! Provides syscalls for creating, mapping, unmapping, and destroying shared memory regions.

use x86_64::{
    VirtAddr,
    structures::paging::{Mapper, Page, PageTableFlags, Size4KiB},
};

use crate::{
    memory::shared::{SharedMemory, SharedMemoryError},
    syscalls::{Errno, memory::find_free_virtual_address_atomic},
    thread::{MappingType, MemoryMapping, scheduler::sched},
};

// Protection flags (match Linux)
#[expect(unused)]
const PROT_READ: u64 = 0x1;
const PROT_WRITE: u64 = 0x2;
const PROT_EXEC: u64 = 0x4;

/// Create a new shared memory region
///
/// # Arguments
/// * `size` - Size of the shared memory region in bytes
///
/// # Returns
/// * Shared memory ID on success
/// * -1 on error (errno set)
pub fn sys_shm_create(size: u64) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if size == 0 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    match SharedMemory::new(size as usize) {
        Ok(shm) => shm.id() as i64,
        Err(SharedMemoryError::InvalidSize) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
        Err(SharedMemoryError::AllocationFailed) => {
            info.lock().errno = Errno::ENOMEM;
            -1
        }
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

/// Map a shared memory region into the calling process's address space
///
/// # Arguments
/// * `shm_id` - Shared memory ID
/// * `addr_hint` - Suggested address (0 for kernel to choose)
/// * `prot` - Protection flags (PROT_READ, PROT_WRITE, PROT_EXEC)
///
/// # Returns
/// * Virtual address of the mapping on success
/// * -1 on error (errno set)
pub fn sys_shm_map(shm_id: u64, addr_hint: u64, prot: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    // Look up the shared memory region
    let shm = match SharedMemory::get(shm_id) {
        Some(shm) => shm,
        None => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let size = shm.size() as u64;

    // Determine mapping address
    let map_addr = if addr_hint == 0 {
        let guard = info.lock();
        find_free_virtual_address_atomic(&guard.memory_mappings, &guard.next_mmap_addr, size)
    } else {
        VirtAddr::new(addr_hint)
    };

    // Convert protection flags to page table flags
    let mut page_flags = PageTableFlags::USER_ACCESSIBLE;
    if prot & PROT_WRITE != 0 {
        page_flags |= PageTableFlags::WRITABLE;
    }
    if prot & PROT_EXEC == 0 {
        page_flags |= PageTableFlags::NO_EXECUTE;
    }

    // Map each frame into the process's address space
    let memory_manager = info.lock().memory_manager.clone();
    {
        let mut manager = memory_manager.lock();
        let frames = shm.frames();

        for (i, frame) in frames.iter().enumerate() {
            let virt_addr = VirtAddr::new(map_addr.as_u64() + (i as u64 * 4096));
            let phys_addr = frame.start_address();

            if manager
                .map_address(virt_addr, phys_addr, page_flags)
                .is_err()
            {
                // Rollback: unmap any pages we've already mapped
                for j in 0..i {
                    let rollback_addr = VirtAddr::new(map_addr.as_u64() + (j as u64 * 4096));
                    // Unmap without deallocating the frame (it's shared)
                    let page: Page<Size4KiB> = Page::containing_address(rollback_addr);
                    if let Ok((_, flush)) = manager.mapper.unmap(page) {
                        flush.flush();
                    }
                }
                info.lock().errno = Errno::ENOMEM;
                return !0u64;
            }
        }
    }

    // Increment reference count
    shm.inc_ref();

    // Record the mapping
    info.lock().memory_mappings.lock().insert(
        map_addr,
        MemoryMapping {
            size,
            flags: page_flags,
            mapping_type: MappingType::Shared(shm_id),
        },
    );

    map_addr.as_u64()
}

/// Unmap a shared memory region from the calling process's address space
///
/// # Arguments
/// * `addr` - Virtual address of the mapping
///
/// # Returns
/// * 0 on success
/// * -1 on error (errno set)
pub fn sys_shm_unmap(addr: u64) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let map_addr = VirtAddr::new(addr);

    // Find and remove the mapping
    let mapping = info.lock().memory_mappings.lock().remove(&map_addr);

    match mapping {
        Some(mapping) => {
            match mapping.mapping_type {
                MappingType::Shared(shm_id) => {
                    // Get the shared memory to decrement ref count
                    if let Some(shm) = SharedMemory::get(shm_id) {
                        shm.dec_ref();
                    }

                    // Unmap the pages (but don't deallocate the frames - they're shared)
                    let memory_manager = info.lock().memory_manager.clone();
                    let mut manager = memory_manager.lock();
                    let page_count = (mapping.size + 0xFFF) / 4096;

                    for i in 0..page_count {
                        let virt_addr = VirtAddr::new(addr + i * 4096);
                        let page: Page<Size4KiB> = Page::containing_address(virt_addr);
                        if let Ok((_, flush)) = manager.mapper.unmap(page) {
                            flush.flush();
                        }
                    }

                    0
                }
                MappingType::Anonymous => {
                    // Not a shared memory mapping, restore and return error
                    info.lock().memory_mappings.lock().insert(map_addr, mapping);
                    info.lock().errno = Errno::EINVAL;
                    -1
                }
            }
        }
        None => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

/// Destroy a shared memory region
///
/// The region can only be destroyed if there are no active mappings.
///
/// # Arguments
/// * `shm_id` - Shared memory ID
///
/// # Returns
/// * 0 on success
/// * -1 on error (errno set)
pub fn sys_shm_destroy(shm_id: u64) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    match SharedMemory::destroy(shm_id) {
        Ok(()) => 0,
        Err(SharedMemoryError::NotFound) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
        Err(SharedMemoryError::StillMapped) => {
            // Still has active mappings
            info.lock().errno = Errno::EACCES;
            -1
        }
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}
