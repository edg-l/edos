use alloc::{format, sync::Arc, vec::Vec};

use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{Mapper, Page, PageTableFlags, Size4KiB},
};

use crate::thread::scheduler::{current_thread, current_thread_info};
use crate::{
    debug::lock_order::{RANK_MAPPERS, RANK_USER_MM, RANK_VMAS},
    fs::{page_cache::CachedPage, vfs::fs_by_mount_id},
    log, log_debug,
    memory::{
        frame_allocator::frame_allocator,
        mapper::memory_mapper,
        pat,
        vma::{
            PAGE_SIZE, USER_VA_END, Vma, VmaBacking, VmaError, VmaFlags, VmaProt, page_round_up,
        },
    },
    println, ranked_lock,
    syscalls::Errno,
    thread::{UserThread, UserThreadInfo, irqlock::IrqSpinlock, pipe::FileDescriptor},
};
use spin::RwLock;

// Protection flags (match Linux)
const PROT_READ: u32 = 0x1;
const PROT_WRITE: u32 = 0x2;
const PROT_EXEC: u32 = 0x4;

// Mapping flags
pub const MAP_SHARED: u32 = 0x01;
pub const MAP_PRIVATE: u32 = 0x02;
/// Unimplemented: the kernel always picks the range for a hinted address.
#[allow(dead_code)]
pub const MAP_FIXED: u32 = 0x10;
pub const MAP_ANONYMOUS: u32 = 0x20;
pub const MAP_PHYSICAL: u32 = 0x40;
pub const MAP_WRITE_COMBINING: u32 = 0x80;

// msync flags
pub const MS_ASYNC: u32 = 0x1;
pub const MS_SYNC: u32 = 0x2;
/// Unimplemented: there is no second cache of a mapping to invalidate.
#[allow(dead_code)]
pub const MS_INVALIDATE: u32 = 0x4;

/// Places a VMA and returns the address it covers, or `None` after setting
/// `errno` on the calling thread.
///
/// With `addr == 0` the kernel picks the range; the search and the insert happen
/// under one acquisition of the VmaSet lock, because a range is only free while
/// that lock is held. An explicit `addr` is taken at the caller's word as to
/// *which* range it wants, and is inserted under the same lock so two callers
/// naming the same address cannot each believe they own it. It is not taken at
/// the caller's word as to whether that range exists: `addr` and `length` are
/// untrusted, so the range is checked against the user half by `VmaSet::insert`.
pub(super) fn claim_range(
    user_arc: &Arc<RwLock<UserThread>>,
    info: &Arc<IrqSpinlock<UserThreadInfo>>,
    addr: u64,
    length: u64,
    prot: VmaProt,
    flags: VmaFlags,
    backing: VmaBacking,
) -> Option<VirtAddr> {
    let next_mmap_addr = info.lock().next_mmap_addr.clone();
    let result = {
        let user_read = user_arc.read();
        let mut vmas = ranked_lock!(RANK_VMAS, "user.vmas", user_read.vmas);

        if addr == 0 {
            vmas.reserve(&next_mmap_addr, length, prot, flags, backing)
        } else {
            // `addr` is a raw user value: VirtAddr::new panics on a non-canonical
            // one, and the sum can wrap, so both are checked before construction.
            // The end is page-rounded for the same reason `reserve` rounds: a
            // mapping owns every page it touches, and two mappings sharing one
            // page destroy each other on a zero-fill fault or an unmap.
            match page_round_up(length)
                .and_then(|len| addr.checked_add(len))
                .filter(|end| *end <= USER_VA_END)
                .ok_or(VmaError::OutOfUserSpace)
            {
                Ok(end) => {
                    let start = VirtAddr::new(addr);
                    vmas.insert(Vma {
                        start,
                        end: VirtAddr::new(end),
                        prot,
                        flags,
                        backing,
                    })
                    .map(|()| start)
                }
                Err(e) => Err(e),
            }
        }
    };

    match result {
        Ok(start) => Some(start),
        Err(e) => {
            info.lock().errno = match e {
                VmaError::OutOfUserSpace => Errno::EINVAL,
                VmaError::NoSpace => Errno::ENOMEM,
            };
            None
        }
    }
}

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
    log_debug!(
        "mmap: addr={addr:#x} len={length:#x} ({} KiB) prot={prot_str} {kind}",
        length / 1024
    );
    let info = current_thread_info();

    info.lock().errno = Errno::Clear;

    if length == 0 {
        info.lock().errno = Errno::EINVAL;
        return !0u64; // -1 (EINVAL)
    }

    // A mapping starts at a page boundary, so an address that is not one names a
    // range the MMU cannot give. Rounding it silently would hand back memory the
    // caller did not ask for and can overlap a neighbour.
    if !addr.is_multiple_of(PAGE_SIZE) {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    let is_physical = (flags & MAP_PHYSICAL) != 0;

    // Access VmaSet from UserThread
    let thread = match current_thread() {
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

        // Claim the range before mapping it, so no other thread can pick the same
        // one while these page tables are being written.
        let map_addr = claim_range(
            &user_arc,
            &info,
            addr,
            length,
            vma_prot,
            VmaFlags::PRIVATE,
            VmaBacking::Physical {
                phys_base: phys_addr,
            },
        );
        let Some(map_addr) = map_addr else {
            return !0u64;
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
        let mut mm = ranked_lock!(RANK_USER_MM, "user.mm", memory_manager);
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
                // Give the claimed range back; nothing is mapped there now.
                {
                    let _user = user_arc.read();
                    ranked_lock!(RANK_VMAS, "user.vmas", _user.vmas).remove(&map_addr);
                }
                info.lock().errno = Errno::ENOMEM;
                return !0u64;
            }
        }
        drop(mm);

        log_debug!("mmap: mapped physical at {map_addr:p}");
        map_addr.as_u64()
    } else if (flags & MAP_ANONYMOUS) != 0 || r8 == u64::MAX {
        // Anonymous mapping (MAP_ANONYMOUS set, or fd == -1).
        if (flags & MAP_PRIVATE) == 0 && (flags & MAP_SHARED) == 0 {
            println!("mmap: anonymous mapping must have MAP_PRIVATE or MAP_SHARED");
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

        // Lazy allocation - just record the VMA, don't allocate frames
        let map_addr = claim_range(
            &user_arc,
            &info,
            addr,
            length,
            vma_prot,
            VmaFlags::PRIVATE | VmaFlags::LAZY,
            VmaBacking::Anonymous,
        );
        let Some(map_addr) = map_addr else {
            return !0u64;
        };

        log_debug!("mmap: lazy mapped at {map_addr:p}");
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

        // Note: no explicit FS pin here. The VMA holds `Arc<VfsInode>`, which
        // bumps the inode refcount; evict_inode fires only when the final Arc
        // (including this VMA's) is released. This is the Linux `i_count` model.
        //
        // The page vector is built before the range is claimed, so the allocation
        // stays outside the VmaSet lock.
        let backing = VmaBacking::FileBacked {
            inode: Arc::clone(&inode),
            file_offset,
            shared: is_shared,
            writable_mapping,
            pages: alloc::vec![None; num_pages],
        };
        let Some(map_addr) =
            claim_range(&user_arc, &info, addr, length, vma_prot, vma_flags, backing)
        else {
            return !0u64;
        };

        // D.1: Register this process in the inode's reverse map so that a future
        // truncate can walk all mappers and unmap PTEs past the new EOF.
        // Entries are Weak to avoid pinning the process indefinitely; tombstones
        // are cleaned up lazily during truncate invalidation.
        {
            let weak = Arc::downgrade(&user_arc);
            let mut mappers = ranked_lock!(RANK_MAPPERS, "inode.mappers", inode.mappers);
            let already = mappers.iter().any(|w| {
                w.upgrade()
                    .map(|a| Arc::ptr_eq(&a, &user_arc))
                    .unwrap_or(false)
            });
            if !already {
                mappers.push(weak);
            }
        }

        if is_shared {
            log_debug!(
                "mmap: file-backed MAP_SHARED at {map_addr:p} len={length:#x} off={file_offset:#x}"
            );
        } else {
            log_debug!(
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
    let info = current_thread_info();
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

    let thread = match current_thread() {
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
        let vmas = ranked_lock!(RANK_VMAS, "user.vmas", user_read.vmas);

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
    //
    // One bulk call per inode, in ascending page order, because the filesystem
    // turns a contiguous run of pages into a single device command. Written one
    // page at a time, a 4 MiB mapping is a thousand synchronous round trips at
    // queue depth one, which is where the several hundred milliseconds a first
    // msync used to cost came from.
    //
    // `dirty_keys` is left alone, as the per-page path also left it: the entry
    // survives a page being cleaned here and costs a later writeback pass one
    // redundant write of a clean page.
    work.sort_by_key(|(inode, page_idx, _)| (inode.mount_id, inode.ino, *page_idx));

    for group in work.chunk_by(|(a, _, _), (b, _, _)| a.mount_id == b.mount_id && a.ino == b.ino) {
        let inode = &group[0].0;
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

        let pages: Vec<(u64, Arc<CachedPage>)> = group
            .iter()
            .map(|(_, page_idx, page)| (*page_idx, Arc::clone(page)))
            .collect();

        // Pinned across the flush so the frames cannot be reclaimed while the
        // device is reading them.
        for (_, page) in &pages {
            page.pin();
        }
        let result = pc_ops.flush_pages_bulk(inode.ino, &pages, None);
        for (_, page) in &pages {
            page.unpin();
        }

        match result {
            Ok(()) => {
                for (_, page) in &pages {
                    page.clear_dirty();
                }
            }
            Err(e) => {
                log!("msync: flush_pages_bulk ino={} err={:?}", inode.ino, e);
                info.lock().errno = Errno::EIO;
                return -1;
            }
        }
    }

    0
}

pub fn sys_munmap(addr: u64, length: u64) -> i32 {
    log_debug!("Unmapping {addr} {length}");
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if length == 0 || !addr.is_multiple_of(PAGE_SIZE) {
        info.lock().errno = Errno::EINVAL;
        return -1; // EINVAL
    }

    let map_addr = VirtAddr::new(addr);

    let thread = match current_thread() {
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

    // A partial page cannot be unmapped, so the range covers every page it
    // touches, matching the rounding `claim_range` applied when it was mapped.
    let Some(end) = page_round_up(length).and_then(|len| addr.checked_add(len)) else {
        info.lock().errno = Errno::EINVAL;
        return -1;
    };
    let unmap_end = VirtAddr::new(end);

    // Remove all VMAs fully covered by [map_addr, unmap_end), splitting straddlers.
    let removed_vmas = {
        let _user = user_arc.read();
        ranked_lock!(RANK_VMAS, "user.vmas", _user.vmas).remove_range(map_addr, unmap_end)
    };

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
                    let mut mm = ranked_lock!(RANK_USER_MM, "user.mm", memory_manager);
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
                if !frames.is_empty() {
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
                crate::memory::tlb::tlb_shootdown(vma_start, vma_page_count);
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
                    let mut mm = ranked_lock!(RANK_USER_MM, "user.mm", memory_manager);
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
                if !frames.is_empty() {
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
                // `pages` Vec drop releases our Arc<CachedPage> refs; the
                // `inode` Arc drops at end of scope. If this was the last ref
                // and the inode was previously orphaned by remove_file,
                // VfsInode::drop triggers FileSystem::evict_inode to free
                // on-disk allocations.
                drop(pages);
                drop(inode);
            }
            VmaBacking::SharedMemory { .. } => {
                // Re-insert and return error; SHM has its own unmap syscall.
                let _user = user_arc.read();
                ranked_lock!(RANK_VMAS, "user.vmas", _user.vmas).insert_validated(vma);
                any_error = true;
            }
            VmaBacking::Tls | VmaBacking::Stack => {
                // Kernel-managed; re-insert and return error.
                let _user = user_arc.read();
                ranked_lock!(RANK_VMAS, "user.vmas", _user.vmas).insert_validated(vma);
                any_error = true;
            }
        }
    }

    if any_error {
        info.lock().errno = Errno::EINVAL;
        log!("Unmap partial error (kernel-managed VMAs in range)");
        return -1;
    }

    log_debug!("Unmap success");
    0
}
