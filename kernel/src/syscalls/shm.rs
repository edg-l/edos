//! Shared memory syscalls
//!
//! Provides syscalls for creating, mapping, unmapping, and destroying shared memory regions.

use x86_64::{
    VirtAddr,
    structures::paging::{Mapper, Page, PageTableFlags, Size4KiB},
};

use crate::syscalls::memory::{
    PROT_EXEC, PROT_WRITE, claim_range, current_user_thread, vma_prot_from,
};
use crate::thread::scheduler::{current_thread, current_thread_info};
use crate::{
    debug::lock_order::{RANK_USER_MM, RANK_VMAS},
    memory::{
        shared::{SharedMemory, SharedMemoryError},
        vma::{VmaBacking, VmaFlags},
    },
    ranked_lock,
    syscalls::Errno,
};

/// Create a shared memory region of `size` bytes and answer with its id.
pub fn sys_shm_create(size: u64) -> Result<u64, Errno> {
    if size == 0 {
        return Err(Errno::EINVAL);
    }

    match SharedMemory::new(size as usize) {
        Ok(shm) => Ok(shm.id()),
        Err(SharedMemoryError::AllocationFailed) => Err(Errno::ENOMEM),
        Err(_) => Err(Errno::EINVAL),
    }
}

/// Map a shared memory region into the caller's address space and answer with
/// the address it landed at.
///
/// `addr_hint` of 0 lets the kernel choose; `prot` is the `PROT_*` set.
pub fn sys_shm_map(shm_id: u64, addr_hint: u64, prot: u32) -> Result<u64, Errno> {
    let info = current_thread_info();
    let shm = SharedMemory::get(shm_id).ok_or(Errno::EINVAL)?;

    let size = shm.size() as u64;

    let thread = match current_thread() {
        Some(t) => t,
        None => {
            return Err(Errno::EINVAL);
        }
    };
    let user_arc = match &thread.user {
        Some(u) => u.clone(),
        None => {
            return Err(Errno::EINVAL);
        }
    };

    // Validate a user-supplied address: must be page-aligned and in user space
    if addr_hint != 0 && (addr_hint & 0xFFF != 0 || addr_hint >= 0x0000_8000_0000_0000) {
        return Err(Errno::EINVAL);
    }

    let vma_prot = vma_prot_from(prot);

    // Claim the range before mapping frames into it, so a concurrent attach
    // cannot pick the same one.
    let map_addr = claim_range(
        &user_arc,
        &info,
        addr_hint,
        size,
        vma_prot,
        VmaFlags::SHARED,
        VmaBacking::SharedMemory { shm_id },
    )?;

    // Hands the claimed range back on a failure path.
    let unclaim = || {
        let user_read = user_arc.read();
        ranked_lock!(RANK_VMAS, "shm::unclaim", user_read.vmas).remove(&map_addr);
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
                unclaim();
                return Err(Errno::ENOMEM);
            }
        }
    }

    // Increment reference count (fails if region is marked for destruction)
    if shm.inc_ref().is_err() {
        // Rollback: unmap all pages we just mapped
        let memory_manager = info.lock().memory_manager.clone();
        let mut manager = ranked_lock!(RANK_USER_MM, "user.mm", memory_manager);
        let page_count = size.div_ceil(4096);
        for i in 0..page_count {
            let virt_addr = VirtAddr::new(map_addr.as_u64() + i * 4096);
            let page: Page<Size4KiB> = Page::containing_address(virt_addr);
            if let Ok((_, flush)) = manager.mapper.unmap(page) {
                flush.flush();
            }
        }
        drop(manager);
        unclaim();
        return Err(Errno::EINVAL);
    }

    Ok(map_addr.as_u64())
}

/// Unmap a shared memory region from the caller's address space.
pub fn sys_shm_unmap(addr: u64) -> Result<u64, Errno> {
    let info = current_thread_info();
    let map_addr = VirtAddr::new(addr);

    let user_arc = current_user_thread()?;

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
                    let page_count = vma.size().div_ceil(4096);
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
                    crate::memory::tlb::tlb_shootdown(VirtAddr::new(addr), page_count);

                    Ok(0)
                }
                _ => {
                    // Not a shared memory mapping, restore and return error
                    let _user = user_arc.read();
                    ranked_lock!(RANK_VMAS, "user.vmas", _user.vmas).insert_validated(vma);
                    Err(Errno::EINVAL)
                }
            }
        }
        None => Err(Errno::EINVAL),
    }
}

/// The size in bytes of a shared memory region.
pub fn sys_shm_size(shm_id: u64) -> Result<u64, Errno> {
    match SharedMemory::get(shm_id) {
        Some(shm) => Ok(shm.size() as u64),
        None => Err(Errno::EINVAL),
    }
}

/// Destroy a shared memory region, which succeeds only once nothing maps it.
pub fn sys_shm_destroy(shm_id: u64) -> Result<u64, Errno> {
    match SharedMemory::destroy(shm_id) {
        Ok(()) => Ok(0),
        // Already marked for destruction.
        Err(SharedMemoryError::Destroyed) => Ok(0),
        Err(_) => Err(Errno::EINVAL),
    }
}
