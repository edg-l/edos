use alloc::{format, sync::Arc, vec::Vec};

use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{Mapper, Page, PageTableFlags, Size4KiB},
};

use crate::{
    fs::{page_cache::CachedPage, vfs::fs_by_mount_id},
    log,
    memory::{
        frame_allocator::frame_allocator,
        mapper::memory_mapper,
        pat,
        vma::{Vma, VmaBacking, VmaFlags, VmaProt},
    },
    println,
    syscalls::Errno,
    thread::{pipe::FileDescriptor, scheduler::sched},
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
        // File-backed mapping.
        let file_offset = r9;
        let fd = r8 as i64;

        let is_shared = (flags & MAP_SHARED) != 0;
        let is_private = (flags & MAP_PRIVATE) != 0;

        if !is_shared && !is_private {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
        if is_shared && is_private {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }

        // Alignment checks.
        if file_offset & 0xFFF != 0 {
            log!("mmap: file_offset must be page-aligned");
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
        if length & 0xFFF != 0 || addr != 0 && addr & 0xFFF != 0 {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }

        // Look up the FsFile from the fd table.
        let fd_table = info.lock().fd_table.clone();
        let fs_file = match fd_table.lock().get_fd(fd as u64).cloned() {
            Some(FileDescriptor::FsFile(f)) => f,
            _ => {
                log!("mmap: invalid fd {fd}");
                info.lock().errno = Errno::EBADF;
                return !0u64;
            }
        };

        // Permission checks: MAP_PRIVATE and MAP_SHARED both require a
        // readable fd. MAP_SHARED + PROT_WRITE additionally requires a
        // writable fd (writes would reach disk).
        if !fs_file.mode.readable() {
            info.lock().errno = Errno::EACCES;
            return !0u64;
        }
        if is_shared && (prot & PROT_WRITE) != 0 && !fs_file.mode.writable() {
            info.lock().errno = Errno::EACCES;
            return !0u64;
        }

        // Get the inode and verify the filesystem supports the page cache.
        let inode = match &fs_file.inode {
            Some(i) => i.clone(),
            None => {
                log!("mmap: file has no inode (virtual fs?)");
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }
        };

        // Verify the filesystem has PageCacheOps (EFS has it; FAT32/memfs do not).
        {
            let maybe_fs = fs_by_mount_id(inode.mount_id);
            match maybe_fs {
                Some(fs) if fs.as_page_cache_ops().is_some() => {}
                _ => {
                    log!("mmap: filesystem does not support page-cached mmap");
                    info.lock().errno = Errno::EINVAL;
                    return !0u64;
                }
            }
        }

        let num_pages = (length / 4096) as usize;

        let map_addr = if addr == 0 {
            let user_read = user_arc.read();
            let next_mmap_addr = info.lock().next_mmap_addr.clone();
            let vmas = user_read.vmas.lock();
            vmas.find_free_address(&next_mmap_addr, length)
        } else {
            VirtAddr::new(addr)
        };

        let writable_mapping = (prot & PROT_WRITE) != 0;

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

        let vma_flags = if is_shared {
            VmaFlags::SHARED | VmaFlags::LAZY
        } else {
            VmaFlags::PRIVATE | VmaFlags::LAZY
        };

        user_arc.read().vmas.lock().insert(Vma {
            start: map_addr,
            end: map_addr + length,
            prot: vma_prot,
            flags: vma_flags,
            backing: VmaBacking::FileBacked {
                inode: Arc::clone(&inode),
                file_offset,
                shared: is_shared,
                writable_mapping,
                pages: alloc::vec![None; num_pages],
            },
        });

        // D.1: Register this process in the inode's reverse map so that a future
        // truncate can walk all mappers and unmap PTEs past the new EOF.
        // Entries are Weak to avoid pinning the process indefinitely; tombstones
        // are cleaned up lazily during truncate invalidation.
        {
            let weak = Arc::downgrade(&user_arc);
            let mut mappers = inode.mappers.lock();
            let already = mappers.iter().any(|w| {
                w.upgrade()
                    .map(|a| Arc::ptr_eq(&a, &user_arc))
                    .unwrap_or(false)
            });
            if !already {
                mappers.push(weak);
            }
        }

        // Notify the filesystem that a new FileBacked VMA has been created.
        // FAT32 uses this to increment its mapper pin count so that a concurrent
        // remove_file defers cluster chain freeing until the last mapper unpins.
        crate::fs::vfs::on_pin(inode.mount_id, inode.ino);

        if is_shared {
            log!(
                "mmap: file-backed MAP_SHARED at {map_addr:p} len={length:#x} off={file_offset:#x}"
            );
        } else {
            log!(
                "mmap: file-backed MAP_PRIVATE at {map_addr:p} len={length:#x} off={file_offset:#x}"
            );
        }
        map_addr.as_u64()
    }
}

/// Flush dirty shared pages for a FileBacked VMA before unmapping.
/// Errors are logged but do not prevent unmap (unmap must not fail).
///
/// `file_offset` is the VMA's file_offset field (page-aligned); the file
/// page index for slot N is `file_offset/4096 + N`.
pub fn flush_shared_vma_pages(
    inode: &Arc<crate::fs::inode::VfsInode>,
    file_offset: u64,
    pages: &[Option<Arc<CachedPage>>],
) {
    // Collect (page_idx, Arc<CachedPage>) for dirty slots.
    let base_idx = file_offset / 4096;
    let mut work: Vec<(u64, Arc<CachedPage>)> = Vec::new();
    for (slot, maybe_page) in pages.iter().enumerate() {
        if let Some(page) = maybe_page {
            if page.is_dirty() {
                work.push((base_idx + slot as u64, Arc::clone(page)));
            }
        }
    }
    if work.is_empty() {
        return;
    }
    let fs = match fs_by_mount_id(inode.mount_id) {
        Some(f) => f,
        None => {
            log!(
                "flush_shared_vma_pages: no fs for mount_id={}",
                inode.mount_id
            );
            return;
        }
    };
    let pc_ops = match fs.as_page_cache_ops() {
        Some(ops) => ops,
        None => {
            log!("flush_shared_vma_pages: fs has no page_cache_ops");
            return;
        }
    };
    for (idx, page) in &work {
        // Safety: no mutable aliasing; the page frame is exclusively
        // referenced by the cache and mapped read-only in userspace PTEs.
        let buf = unsafe { page.as_slice() };
        match pc_ops.flush_page(inode.ino, *idx, buf, 4096) {
            Ok(()) => page.clear_dirty(),
            Err(e) => log!(
                "flush_shared_vma_pages: flush_page ino={} idx={} err={:?}",
                inode.ino,
                idx,
                e
            ),
        }
    }
}

/// sys_msync: flush MAP_SHARED dirty pages in [addr, addr+len) to storage.
///
/// Lock ordering: VmaSet lock is acquired to collect work (Arc<CachedPage> + fs info),
/// then released before calling flush_page (which does AHCI I/O).
pub fn sys_msync(addr: u64, len: u64, flags: u32) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    // MS_ASYNC: no-op for v1; pages are already marked dirty and the writeback
    // kthread will flush them on its next periodic pass.
    if flags & MS_ASYNC != 0 && flags & MS_SYNC == 0 {
        return 0;
    }
    // MS_INVALIDATE: no-op for v1 (no revalidation path yet).

    if addr & 0xFFF != 0 || len & 0xFFF != 0 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

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

    let range_start = VirtAddr::new(addr);
    let range_end = VirtAddr::new(addr + len);

    // Collect flush work while holding the VmaSet lock, then flush after releasing.
    // Each work item: (inode Arc, file page index, CachedPage Arc).
    let mut work: Vec<(Arc<crate::fs::inode::VfsInode>, u64, Arc<CachedPage>)> = Vec::new();

    {
        let user_read = user_arc.read();
        let vmas = user_read.vmas.lock();

        for vma in vmas.iter() {
            // Skip VMAs that don't overlap [range_start, range_end)
            if vma.end <= range_start || vma.start >= range_end {
                continue;
            }
            let (inode, file_offset, pages) = match &vma.backing {
                VmaBacking::FileBacked {
                    inode,
                    file_offset,
                    shared: true,
                    pages,
                    ..
                } => (inode, *file_offset, pages),
                // Skip non-shared or non-file VMAs silently (Linux behavior).
                _ => continue,
            };

            // Intersection of [range_start, range_end) and [vma.start, vma.end)
            let effective_start = range_start.max(vma.start);
            let effective_end = range_end.min(vma.end);

            let first_slot = ((effective_start.as_u64() - vma.start.as_u64()) / 4096) as usize;
            let last_slot = ((effective_end.as_u64() - vma.start.as_u64() + 4095) / 4096) as usize;
            let last_slot = last_slot.min(pages.len());

            for slot in first_slot..last_slot {
                if let Some(page) = &pages[slot] {
                    if page.is_dirty() {
                        let page_idx = (file_offset + slot as u64 * 4096) / 4096;
                        work.push((Arc::clone(inode), page_idx, Arc::clone(page)));
                    }
                }
            }
        }
        // VmaSet lock dropped here.
    }

    // Flush collected pages outside the VmaSet lock (AHCI I/O may block).
    for (inode, page_idx, page) in &work {
        let fs = match fs_by_mount_id(inode.mount_id) {
            Some(f) => f,
            None => {
                log!("msync: no fs for mount_id={}", inode.mount_id);
                continue;
            }
        };
        let pc_ops = match fs.as_page_cache_ops() {
            Some(ops) => ops,
            None => continue,
        };
        // Safety: no mutable aliasing; user PTEs may be writable but the cache
        // frame is the sole physical backing and we're only reading it here.
        let buf = unsafe { page.as_slice() };
        match pc_ops.flush_page(inode.ino, *page_idx, buf, 4096) {
            Ok(()) => page.clear_dirty(),
            Err(e) => {
                log!(
                    "msync: flush_page ino={} idx={} err={:?}",
                    inode.ino,
                    page_idx,
                    e
                );
                info.lock().errno = Errno::EIO;
                return -1;
            }
        }
    }

    0
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

    let unmap_end = VirtAddr::new(addr + length);

    // Remove all VMAs fully covered by [map_addr, unmap_end), splitting straddlers.
    let removed_vmas = user_arc
        .read()
        .vmas
        .lock()
        .remove_range(map_addr, unmap_end);

    if removed_vmas.is_empty() {
        log!("Unmap fail, no VMAs in range");
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    let memory_manager = info.lock().memory_manager.clone();
    let mut any_error = false;

    for vma in removed_vmas {
        let vma_start = vma.start;
        let vma_length = vma.size();
        let vma_page_count = (vma_length + 0xFFF) / 4096;

        match vma.backing {
            VmaBacking::Anonymous => {
                let mut frames = alloc::vec::Vec::new();
                {
                    let mut mm = memory_manager.lock();
                    for i in 0..vma_page_count {
                        let virt = VirtAddr::new(vma_start.as_u64() + i * 4096);
                        let page: Page<Size4KiB> = Page::containing_address(virt);
                        if let Ok(phys) = mm.mapper.translate_page(page) {
                            if let Ok((_, flush)) = mm.mapper.unmap(page) {
                                flush.ignore();
                                frames.push(phys);
                            }
                        }
                    }
                }
                if !frames.is_empty() && crate::memory::tlb::shootdown_needed() {
                    crate::memory::tlb::tlb_shootdown(vma_start, vma_page_count);
                }
                let mut fa = frame_allocator();
                for frame in frames {
                    unsafe { fa.deallocate_frame(frame) };
                }
            }
            VmaBacking::Physical { .. } => {
                {
                    let mut mapper = memory_mapper();
                    for i in 0..vma_page_count {
                        let virt = VirtAddr::new(vma_start.as_u64() + i * 4096);
                        let page: Page<Size4KiB> = Page::containing_address(virt);
                        if let Ok((_, flush)) = mapper.mapper.unmap(page) {
                            flush.flush();
                        }
                    }
                }
                if crate::memory::tlb::shootdown_needed() {
                    crate::memory::tlb::tlb_shootdown(vma_start, vma_page_count);
                }
            }
            VmaBacking::FileBacked {
                inode,
                file_offset,
                shared,
                pages,
                ..
            } => {
                // For MAP_SHARED: flush dirty pages to disk before unmapping.
                // Flush errors are logged but do not prevent unmap (unmap must not fail).
                if shared {
                    flush_shared_vma_pages(&inode, file_offset, &pages);
                }

                // Unmap PTEs and decrement frame refcounts for all present pages.
                // Drop the per-page Arc<CachedPage> (done automatically by `pages` Vec drop)
                // which keeps the cache frame alive for the duration of the mapping.
                let mut frames = alloc::vec::Vec::new();
                {
                    let mut mm = memory_manager.lock();
                    for i in 0..vma_page_count {
                        let virt = VirtAddr::new(vma_start.as_u64() + i * 4096);
                        let page: Page<Size4KiB> = Page::containing_address(virt);
                        if let Ok(phys) = mm.mapper.translate_page(page) {
                            if let Ok((_, flush)) = mm.mapper.unmap(page) {
                                flush.ignore();
                                frames.push(phys);
                            }
                        }
                    }
                }
                if !frames.is_empty() && crate::memory::tlb::shootdown_needed() {
                    crate::memory::tlb::tlb_shootdown(vma_start, vma_page_count);
                }
                {
                    let mut fa = frame_allocator();
                    for frame in frames {
                        // Decrement the refcount that was bumped at fault-in time.
                        // When rc reaches 0 the frame is still alive via the
                        // CachedPage Arc held in `pages`; the Arc drop above
                        // (via `pages` Vec drop) will then be the last reference.
                        fa.dec_refcount(frame);
                    }
                }
                // Capture (mount_id, ino) before dropping the inode Arc.
                let (mount_id, ino) = (inode.mount_id, inode.ino);
                // `pages` Vec is dropped here, releasing Arc<CachedPage> refs.
                drop(pages);
                // Notify the filesystem that one FileBacked VMA has been unmapped.
                // FAT32 uses this to decrement its mapper pin count and free orphan
                // cluster chains when the last mapper unpins an unlinked file.
                // Must be called after `drop(pages)` so page refs are released first.
                crate::fs::vfs::on_unpin(mount_id, ino);
            }
            VmaBacking::SharedMemory { .. } => {
                // Re-insert and return error; SHM has its own unmap syscall.
                user_arc.read().vmas.lock().insert(vma);
                any_error = true;
            }
            VmaBacking::Tls | VmaBacking::Stack => {
                // Kernel-managed; re-insert and return error.
                user_arc.read().vmas.lock().insert(vma);
                any_error = true;
            }
        }
    }

    if any_error {
        info.lock().errno = Errno::EINVAL;
        log!("Unmap partial error (kernel-managed VMAs in range)");
        return -1;
    }

    log!("Unmap success");
    0
}
