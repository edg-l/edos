//! Shared memory syscalls
//!
//! Provides syscalls for creating, mapping, unmapping, and destroying shared memory regions.

use x86_64::{
    VirtAddr,
    structures::paging::{Mapper, Page, PageTableFlags, Size4KiB},
};

use crate::{
    debug::lock_order::{RANK_USER_MM, RANK_VMAS},
    memory::{
        shared::{SharedMemory, SharedMemoryError},
        vma::{Vma, VmaBacking, VmaFlags, VmaProt},
    },
    ranked_lock,
    syscalls::Errno,
    thread::scheduler::sched,
};

// Protection flags (match Linux)
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

    // Determine mapping address
    let map_addr = if addr_hint == 0 {
        let user_read = user_arc.read();
        let next_mmap_addr = info.lock().next_mmap_addr.clone();
        let vmas = ranked_lock!(RANK_VMAS, "user.vmas", user_read.vmas);
        vmas.find_free_address(&next_mmap_addr, size)
    } else {
        // Validate user-supplied address: must be page-aligned and in user space
        if addr_hint & 0xFFF != 0 || addr_hint >= 0x0000_8000_0000_0000 {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
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
        let mut manager = ranked_lock!(RANK_USER_MM, "user.mm", memory_manager);
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

    // Increment reference count (fails if region is marked for destruction)
    if shm.inc_ref().is_err() {
        // Rollback: unmap all pages we just mapped
        let memory_manager = info.lock().memory_manager.clone();
        let mut manager = ranked_lock!(RANK_USER_MM, "user.mm", memory_manager);
        let page_count = (size + 0xFFF) / 4096;
        for i in 0..page_count {
            let virt_addr = VirtAddr::new(map_addr.as_u64() + i * 4096);
            let page: Page<Size4KiB> = Page::containing_address(virt_addr);
            if let Ok((_, flush)) = manager.mapper.unmap(page) {
                flush.flush();
            }
        }
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

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

    // Record the mapping in VmaSet
    {
        let _user = user_arc.read();
        ranked_lock!(RANK_VMAS, "user.vmas", _user.vmas).insert(Vma {
            start: map_addr,
            end: map_addr + size,
            prot: vma_prot,
            flags: VmaFlags::SHARED,
            backing: VmaBacking::SharedMemory { shm_id },
        });
    }

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

    // Find and remove the VMA
    let vma = {
        let _user = user_arc.read();
        ranked_lock!(RANK_VMAS, "user.vmas", _user.vmas).remove(&map_addr)
    };

    match vma {
        Some(vma) => {
            match &vma.backing {
                VmaBacking::SharedMemory { shm_id } => {
                    let shm_id = *shm_id;
                    // Get the shared memory to decrement ref count
                    if let Some(shm) = SharedMemory::get(shm_id) {
                        shm.dec_ref();
                    }

                    // Unmap the pages (but don't deallocate the frames - they're shared)
                    let memory_manager = info.lock().memory_manager.clone();
                    let page_count = (vma.size() + 0xFFF) / 4096;
                    {
                        let mut manager = ranked_lock!(RANK_USER_MM, "user.mm", memory_manager);
                        for i in 0..page_count {
                            let virt_addr = VirtAddr::new(addr + i * 4096);
                            let page: Page<Size4KiB> = Page::containing_address(virt_addr);
                            if let Ok((_, flush)) = manager.mapper.unmap(page) {
                                flush.flush();
                            }
                        }
                    }
                    if crate::memory::tlb::shootdown_needed() {
                        crate::memory::tlb::tlb_shootdown(VirtAddr::new(addr), page_count);
                    }

                    0
                }
                _ => {
                    // Not a shared memory mapping, restore and return error
                    let _user = user_arc.read();
                    ranked_lock!(RANK_VMAS, "user.vmas", _user.vmas).insert(vma);
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

/// Get the size of a shared memory region in bytes.
///
/// # Arguments
/// * `shm_id` - Shared memory ID
///
/// # Returns
/// * Size in bytes on success
/// * -1 on error
pub fn sys_shm_size(shm_id: u64) -> i64 {
    match SharedMemory::get(shm_id) {
        Some(shm) => shm.size() as i64,
        None => -1,
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
        Err(SharedMemoryError::Destroyed) => {
            // Already marked for destruction
            0
        }
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}
