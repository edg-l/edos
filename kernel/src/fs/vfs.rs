use alloc::{
    boxed::Box,
    collections::btree_map::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::{Mutex, RwLock};

use super::{
    Error, File, FileAttrs, FileKind, FileSystem, MmapRegion, MountInfo, StatFs, dentry,
    handle::Pollable, inode::VfsInode, path::Path,
};
use x86_64::{
    VirtAddr,
    structures::paging::{Mapper, Page, Size4KiB},
};

use crate::{
    fs::gpt::FilesystemType,
    memory::{frame_allocator::frame_allocator, mapper::MemoryManager, vma::VmaBacking},
    thread::irqlock::IrqSpinlock,
};

static NEXT_MOUNT_ID: AtomicUsize = AtomicUsize::new(1);

/// Global registry of inodes that have MAP_SHARED dirty pages.
/// Entries are Weak so that inode drop removes them naturally (via tombstoning).
/// The writeback kthread iterates this list to flush dirty shared-mapping pages.
// IrqSpinlock (not plain Mutex) because `register_dirty_inode` is called from
// `fault_in_page`, which in turn runs inside the page-fault handler context.
// With a plain spin::Mutex, the writeback kthread could hold DIRTY_INODES
// while getting preempted, then the same CPU takes a page fault that calls
// `register_dirty_inode` -- deadlock. IrqSpinlock disables interrupts while
// held, preventing the recursive acquire.
static DIRTY_INODES: IrqSpinlock<Vec<alloc::sync::Weak<VfsInode>>> = IrqSpinlock::new(Vec::new());

pub struct MountEntry {
    pub fs: Arc<dyn FileSystem + Send + Sync>,
    pub mount_id: usize,
    pub device_id: usize,
    pub partition_index: usize,
    pub filesystem: FilesystemType,
}

static VFS: RwLock<BTreeMap<Path, MountEntry>> = RwLock::new(BTreeMap::new());

/// Result of a VFS lookup: the filesystem, mount-relative path, and mount ID.
/// Internal type used by resolve functions.
struct VfsLookup {
    fs: Arc<dyn FileSystem + Send + Sync>,
    relative: Path,
    mount_id: usize,
}

/// A resolved filesystem operation handle.
pub struct VfsOp {
    pub fs: Arc<dyn FileSystem + Send + Sync>,
    pub relative: Path,
    pub inode: Option<Arc<VfsInode>>,
    pub mount_id: usize,
}

/// Look up the filesystem for a given path.
/// Uses longest-prefix matching to find the deepest mount point.
fn lookup(path: &Path) -> Option<VfsLookup> {
    let registry = VFS.read();
    let mut best_mount: Option<(&Path, &MountEntry)> = None;
    for (mount_path, entry) in registry.iter() {
        if path == mount_path || path.starts_with(mount_path) {
            match best_mount {
                None => best_mount = Some((mount_path, entry)),
                Some((prev, _)) if mount_path.component_count() > prev.component_count() => {
                    best_mount = Some((mount_path, entry));
                }
                _ => {}
            }
        }
    }
    let (mount_path, entry) = best_mount?;
    let relative = path.strip_prefix(mount_path).normalize();
    Some(VfsLookup {
        fs: entry.fs.clone(),
        relative,
        mount_id: entry.mount_id,
    })
}

/// Look up for FileInfo specifically - handles the case where the path itself is a mount point
/// by resolving through the parent filesystem.
fn lookup_for_info(path: &Path) -> Option<VfsLookup> {
    if VFS.read().contains_key(path) {
        if let Some(parent) = path.parent() {
            return lookup(&parent).map(|lk| {
                let name = path.last_component().unwrap_or("");
                let relative = lk.relative.join(name).normalize();
                VfsLookup {
                    fs: lk.fs,
                    relative,
                    mount_id: lk.mount_id,
                }
            });
        }
    }
    lookup(path)
}

/// Resolve a path to its VfsInode, using the dentry cache.
/// On cache miss, calls the filesystem's resolve_inode + file_info to populate.
fn resolve_inode_for(
    mount_id: usize,
    fs: &Arc<dyn FileSystem + Send + Sync>,
    relative: &Path,
) -> Option<Arc<VfsInode>> {
    let dc = dentry::dentry_cache();

    if let Some(inode) = dc.lookup(mount_id, relative) {
        return Some(inode);
    }

    let ino = fs.resolve_inode(relative).ok()?;
    let info = fs.file_info(relative).ok()?;
    let inode = VfsInode::new(mount_id, ino, info.kind);
    dc.insert(mount_id, relative.clone(), inode.clone());
    Some(inode)
}

/// Resolve a path to a VfsOp with inode from dentry cache.
pub fn resolve(path: &Path) -> Option<VfsOp> {
    let lk = lookup(path)?;
    let inode = resolve_inode_for(lk.mount_id, &lk.fs, &lk.relative);
    Some(VfsOp {
        fs: lk.fs,
        relative: lk.relative,
        inode,
        mount_id: lk.mount_id,
    })
}

/// Resolve for file_info - handles mount point parent resolution.
pub fn resolve_for_info(path: &Path) -> Option<VfsOp> {
    let lk = lookup_for_info(path)?;
    let inode = resolve_inode_for(lk.mount_id, &lk.fs, &lk.relative);
    Some(VfsOp {
        fs: lk.fs,
        relative: lk.relative,
        inode,
        mount_id: lk.mount_id,
    })
}

/// Resolve a path to a VfsOp without inode resolution.
/// Used for create/mkdir where the target doesn't exist yet.
pub fn resolve_mount(path: &Path) -> Option<VfsOp> {
    let lk = lookup(path)?;
    Some(VfsOp {
        fs: lk.fs,
        relative: lk.relative,
        inode: None,
        mount_id: lk.mount_id,
    })
}

/// Resolve using a pre-cached inode (from FsFile). Skips dentry lookup.
pub fn resolve_with_inode(path: &Path, inode: Option<Arc<VfsInode>>) -> Option<VfsOp> {
    let lk = lookup(path)?;
    Some(VfsOp {
        fs: lk.fs,
        relative: lk.relative,
        inode,
        mount_id: lk.mount_id,
    })
}

// --- Read-path operations ---

pub fn read(op: &VfsOp, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
    let _guard = op.inode.as_ref().map(|i| i.lock.read());

    if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
        if let Some(pc_ops) = op.fs.as_page_cache_ops() {
            let file_size = op
                .fs
                .file_size_ino(inode.ino)
                .unwrap_or_else(|_| op.fs.file_info(&op.relative).map(|f| f.size).unwrap_or(0))
                as usize;
            if offset >= file_size {
                return Ok(Vec::new());
            }
            let clamped = count.min(file_size - offset);
            return page_cache_read(inode, pc_ops, offset, clamped);
        }
        match op.fs.read_bytes_ino(inode.ino, offset, count) {
            Err(Error::Unsupported) => {}
            result => return result,
        }
    }
    op.fs.read_bytes(&op.relative, offset, count)
}

fn page_cache_read(
    inode: &Arc<VfsInode>,
    pc_ops: &dyn super::page_cache::PageCacheOps,
    offset: usize,
    count: usize,
) -> Result<Vec<u8>, Error> {
    if count == 0 {
        return Ok(Vec::new());
    }

    let start_page = offset / 4096;
    let end_page = (offset + count - 1) / 4096;
    let ino = inode.ino;

    // Check which pages are already cached vs need loading.
    let mut uncached_start: Option<usize> = None;
    let mut uncached_ranges: Vec<(usize, usize)> = Vec::new(); // (start_page, end_page) inclusive

    for page_idx in start_page..=end_page {
        let is_cached = {
            let map = inode.pages.pages.lock();
            map.contains_key(&(page_idx as u64))
        };
        if !is_cached {
            if uncached_start.is_none() {
                uncached_start = Some(page_idx);
            }
        } else if let Some(start) = uncached_start.take() {
            uncached_ranges.push((start, page_idx - 1));
        }
    }
    if let Some(start) = uncached_start {
        uncached_ranges.push((start, end_page));
    }

    // Bulk-fill uncached ranges using fill_page. For contiguous uncached
    // ranges, fill_page is called per page but reads go through the FS
    // driver which can batch them.
    for &(range_start, range_end) in &uncached_ranges {
        let byte_offset = range_start * 4096;
        let byte_count = (range_end - range_start + 1) * 4096;

        // Use fill_pages_bulk if available; otherwise fall back per-page.
        let bulk_data = pc_ops.fill_pages_bulk(ino, byte_offset, byte_count);

        match bulk_data {
            Ok(data) => {
                // Distribute bulk data into per-page cache entries.
                for page_idx in range_start..=range_end {
                    let data_offset = (page_idx - range_start) * 4096;
                    inode.pages.get_or_fill(page_idx as u64, |buf| {
                        let available = data.len().saturating_sub(data_offset);
                        let copy = available.min(4096);
                        if copy > 0 {
                            buf[..copy].copy_from_slice(&data[data_offset..data_offset + copy]);
                        }
                        if copy < 4096 {
                            buf[copy..].fill(0);
                        }
                        Ok(())
                    })?;
                }
            }
            Err(_) => {
                // Fallback: fill per-page
                for page_idx in range_start..=range_end {
                    inode.pages.get_or_fill(page_idx as u64, |buf| {
                        let valid = pc_ops.fill_page(ino, page_idx as u64, buf)?;
                        if valid < 4096 {
                            buf[valid..].fill(0);
                        }
                        Ok(())
                    })?;
                }
            }
        }
    }

    // Now all pages are cached. Read from cache.
    let mut result = Vec::with_capacity(count);
    for page_idx in start_page..=end_page {
        let guard = inode.pages.get_or_fill(page_idx as u64, |buf| {
            // Should always be a cache hit at this point.
            let valid = pc_ops.fill_page(ino, page_idx as u64, buf)?;
            if valid < 4096 {
                buf[valid..].fill(0);
            }
            Ok(())
        })?;

        let page_start_in_file = page_idx * 4096;
        let copy_start = if page_idx == start_page {
            offset - page_start_in_file
        } else {
            0
        };
        let copy_end = if page_idx == end_page {
            offset + count - page_start_in_file
        } else {
            4096
        };
        let copy_end = copy_end.min(4096);

        let slice = unsafe { guard.as_slice() };
        result.extend_from_slice(&slice[copy_start..copy_end]);
    }

    Ok(result)
}

pub fn file_info(op: &VfsOp) -> Result<File, Error> {
    let _guard = op.inode.as_ref().map(|i| i.lock.read());
    op.fs.file_info(&op.relative)
}

pub fn list_files(op: &VfsOp, full_path: &Path) -> Result<Vec<File>, Error> {
    let _guard = op.inode.as_ref().map(|i| i.lock.read());
    let mut files = op.fs.list_files(&op.relative)?;

    // Append synthetic directory entries for child mount points.
    for (name, _mount_path) in child_mount_points(full_path) {
        if !files.iter().any(|f| f.name == name) {
            files.push(File {
                name,
                kind: FileKind::Directory,
                size: 0,
                attrs: FileAttrs {
                    readonly: false,
                    hidden: false,
                    system: false,
                    archive: false,
                },
                created: None,
                accessed: None,
                modified: None,
            });
        }
    }

    Ok(files)
}

// --- Write-path operations ---

/// Write with optional O_APPEND support. The append size query happens
/// inside the write lock, preventing reentrancy deadlocks.
pub fn write(op: &VfsOp, offset: usize, data: &[u8], append: bool) -> Result<u64, Error> {
    let _guard = op.inode.as_ref().map(|i| i.lock.write());
    let actual_offset = if append {
        if let Some(ino) = op.inode.as_ref().map(|i| i.ino).filter(|&i| i != 0) {
            match op.fs.file_size_ino(ino) {
                Ok(size) => size as usize,
                Err(Error::Unsupported) => op
                    .fs
                    .file_info(&op.relative)
                    .map(|f| f.size as usize)
                    .unwrap_or(offset),
                Err(e) => return Err(e),
            }
        } else {
            op.fs
                .file_info(&op.relative)
                .map(|f| f.size as usize)
                .unwrap_or(offset)
        }
    } else {
        offset
    };

    if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
        if let Some(pc_ops) = op.fs.as_page_cache_ops() {
            return page_cache_write(inode, pc_ops, actual_offset, data);
        }
        match op.fs.write_bytes_ino(inode.ino, actual_offset, data) {
            Err(Error::Unsupported) => {}
            result => return result,
        }
    }
    op.fs.write_bytes(&op.relative, actual_offset, data)
}

fn page_cache_write(
    inode: &Arc<VfsInode>,
    pc_ops: &dyn super::page_cache::PageCacheOps,
    offset: usize,
    data: &[u8],
) -> Result<u64, Error> {
    if data.is_empty() {
        return Ok(0);
    }

    let start_page = offset / 4096;
    let end_page = (offset + data.len() - 1) / 4096;
    let ino = inode.ino;
    let mut data_pos = 0usize;

    for page_idx in start_page..=end_page {
        let page_start_in_file = page_idx * 4096;
        let write_start = if page_idx == start_page {
            offset - page_start_in_file
        } else {
            0
        };
        let write_end = if page_idx == end_page {
            offset + data.len() - page_start_in_file
        } else {
            4096
        };
        let write_len = write_end - write_start;

        let is_full_page = write_start == 0 && write_end == 4096;

        let guard = if is_full_page {
            inode.pages.get_or_fill(page_idx as u64, |buf| {
                buf.fill(0);
                Ok(())
            })?
        } else {
            inode.pages.get_or_fill(page_idx as u64, |buf| {
                let valid = pc_ops.fill_page(ino, page_idx as u64, buf)?;
                if valid < 4096 {
                    buf[valid..].fill(0);
                }
                Ok(())
            })?
        };

        let slice = unsafe { guard.as_slice_mut() };
        slice[write_start..write_end].copy_from_slice(&data[data_pos..data_pos + write_len]);
        data_pos += write_len;

        inode.pages.mark_dirty(page_idx as u64);
    }

    // Write-through: flush dirty pages to disk immediately.
    inode
        .pages
        .flush_dirty(|page_index, buf| pc_ops.flush_page(ino, page_index, buf, 4096))?;

    let new_size = (offset + data.len()) as u64;
    pc_ops.update_size(ino, new_size)?;

    // Return bytes written (POSIX write semantics), not new file size. The sys_write
    // caller uses this both as the userspace return and to advance file.offset; using
    // new_size would make offsets compound and cause std write_all to panic when
    // `write` claims it wrote more bytes than the input buffer had.
    Ok(data.len() as u64)
}

pub fn truncate(op: &VfsOp, size: u64) -> Result<(), Error> {
    let _guard = op.inode.as_ref().map(|i| i.lock.write());
    let result = op.fs.truncate(&op.relative, size);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
        if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
            let from_page = (size as usize + 4095) / 4096;
            // D.2: Unmap PTEs past new_size from every process that has a
            // FileBacked VMA on this inode, before freeing the cache frames.
            // Lock ordering: inode.lock (held above) > mappers.lock > vmas.lock
            // > memory_manager.lock.  We release vmas/mm locks before shootdown.
            invalidate_mappings_above(inode, size);
            inode.pages.invalidate_from(from_page as u64);
        }
    }
    result
}

pub fn ioctl(op: &VfsOp, request: u64, arg: u64) -> Result<u64, Error> {
    let _guard = op.inode.as_ref().map(|i| i.lock.write());
    op.fs.ioctl(&op.relative, request, arg)
}

// --- Directory mutation operations ---

fn resolve_parent_inode(op: &VfsOp) -> Option<Arc<VfsInode>> {
    let parent = op.relative.parent()?;
    resolve_inode_for(op.mount_id, &op.fs, &parent)
}

pub fn create_file(op: &VfsOp) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode.as_ref().map(|i| i.lock.write());
    let result = op.fs.create_file(&op.relative);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
    }
    result
}

pub fn create_dir(op: &VfsOp) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode.as_ref().map(|i| i.lock.write());
    let result = op.fs.create_dir(&op.relative);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
    }
    result
}

/// Lock ordering note: `remove_file` acquires parent inode.lock (write) then
/// inside `invalidate_mappings_above` acquires inode.mappers.lock > vmas.lock
/// > memory_manager.lock. This is consistent with truncate's existing contract.
pub fn remove_file(op: &VfsOp) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode.as_ref().map(|i| i.lock.write());
    let result = op.fs.remove_file(&op.relative);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
        if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
            // Actively tear down PTEs for all MAP_SHARED FileBacked VMAs that
            // reference this inode. This prevents subsequent writes through those
            // mappings from silently dirtying a now-deleted file's cluster chain.
            // PTEs for already-faulted MAP_PRIVATE pages are also cleared.
            // Lock ordering: parent inode.lock (held above) > mappers.lock >
            // vmas.lock > memory_manager.lock (acquired inside
            // invalidate_mappings_above).
            // The dentry cache has been invalidated above, so new opens fail ENOENT.
            // Filesystems that implement orphan semantics (FAT32) use mappers_pin to
            // defer cluster chain freeing until the last unmap.
            invalidate_mappings_above(inode, 0);
            inode.pages.invalidate_all();
        }
    }
    result
}

pub fn remove_dir(op: &VfsOp) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode.as_ref().map(|i| i.lock.write());
    let result = op.fs.remove_dir(&op.relative);
    if result.is_ok() {
        let dc = dentry::dentry_cache();
        dc.invalidate(op.mount_id, &op.relative);
        dc.invalidate_children(op.mount_id, &op.relative);
    }
    result
}

pub fn rename(old_op: &VfsOp, new_op: &VfsOp) -> Result<(), Error> {
    if !Arc::ptr_eq(&old_op.fs, &new_op.fs) {
        return Err(Error::Unsupported);
    }

    let old_parent_inode = resolve_parent_inode(old_op);
    let new_parent_inode = resolve_parent_inode(new_op);

    // Acquire locks in inode number order to prevent deadlocks.
    let (_g1, _g2) = match (&old_parent_inode, &new_parent_inode) {
        (Some(a), Some(b)) if Arc::ptr_eq(a, b) => (Some(a.lock.write()), None),
        (Some(a), Some(b)) if a.ino <= b.ino => {
            let g1 = a.lock.write();
            let g2 = b.lock.write();
            (Some(g1), Some(g2))
        }
        (Some(a), Some(b)) => {
            let g1 = b.lock.write();
            let g2 = a.lock.write();
            (Some(g1), Some(g2))
        }
        (Some(a), None) => (Some(a.lock.write()), None),
        (None, Some(b)) => (Some(b.lock.write()), None),
        (None, None) => (None, None),
    };

    let result = old_op.fs.rename(&old_op.relative, &new_op.relative);
    if result.is_ok() {
        let dc = dentry::dentry_cache();
        dc.invalidate_children(old_op.mount_id, &old_op.relative);
        dc.invalidate(old_op.mount_id, &old_op.relative);
        dc.invalidate(new_op.mount_id, &new_op.relative);
    }
    result
}

// --- Passthrough operations (no locking needed) ---

pub fn flush(op: &VfsOp) -> Result<(), Error> {
    op.fs.flush()
}

pub fn flush_file(op: &VfsOp) -> Result<(), Error> {
    if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
        // Flush per-inode dirty page cache pages first.
        if let Some(pc_ops) = op.fs.as_page_cache_ops() {
            let ino = inode.ino;
            inode
                .pages
                .flush_dirty(|page_index, buf| pc_ops.flush_page(ino, page_index, buf, 4096))?;
        }
        match op.fs.flush_inode(inode.ino) {
            Err(Error::Unsupported) => {}
            result => return result,
        }
    }
    op.fs.flush()
}

pub fn poll(op: &VfsOp) -> Result<Box<dyn Pollable>, Error> {
    op.fs.poll(&op.relative)
}

pub fn mmap(
    op: &VfsOp,
    offset: usize,
    length: usize,
    memory: Arc<Mutex<MemoryManager>>,
) -> Result<MmapRegion, Error> {
    op.fs.mmap(&op.relative, offset, length, memory)
}

pub fn statfs(op: &VfsOp) -> Result<StatFs, Error> {
    op.fs.statfs()
}

// --- Mount table management ---

pub fn mount(mount_point: Path, mut entry: MountEntry) {
    entry.mount_id = NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed);
    VFS.write().insert(mount_point, entry);
}

pub fn unmount(mount_point: &Path) -> bool {
    VFS.write().remove(mount_point).is_some()
}

pub fn list_mounts() -> Vec<MountInfo> {
    VFS.read()
        .iter()
        .map(|(path, entry)| MountInfo {
            mount_point: path.clone(),
            device_id: entry.device_id,
            partition_index: entry.partition_index,
            filesystem: entry.filesystem.clone(),
        })
        .collect()
}

/// Returns the names and full paths of direct child mount points under `parent`.
/// Used by list_files to synthesize directory entries for mount points.
pub fn child_mount_points(parent: &Path) -> Vec<(String, Path)> {
    let registry = VFS.read();
    let parent_depth = parent.component_count();
    let mut children = Vec::new();
    for (mount_path, _) in registry.iter() {
        if mount_path.starts_with(parent) && mount_path.component_count() == parent_depth + 1 {
            if let Some(name) = mount_path.last_component() {
                children.push((name.to_string(), mount_path.clone()));
            }
        }
    }
    children
}

/// Check if a path is a registered mount point.
pub fn is_mount_point(path: &Path) -> bool {
    VFS.read().contains_key(path)
}

/// Called when a new FileBacked VMA is created (mmap) for `ino` on `mount_id`.
///
/// Resolves the filesystem and delegates to `FileSystem::on_pin(ino)`.
/// Errors are swallowed; mmap has already succeeded by the time this is called.
pub fn on_pin(mount_id: usize, ino: u64) {
    if let Some(fs) = fs_by_mount_id(mount_id) {
        let _ = fs.on_pin(ino);
    }
}

/// Called on VMA drop (munmap or process exit) for FileBacked VMAs.
///
/// Resolves the filesystem for `mount_id` and delegates to
/// `FileSystem::on_unpin(ino)`. Filesystems that implement orphan-preserve
/// semantics (FAT32) will decrement their mapper pin count and free the cluster
/// chain when the last mapper unpins an unlinked file.
///
/// Errors are swallowed here because unmap must never fail. Callers log
/// unexpected errors separately if needed.
pub fn on_unpin(mount_id: usize, ino: u64) {
    if let Some(fs) = fs_by_mount_id(mount_id) {
        let _ = fs.on_unpin(ino);
    }
}

/// Look up the filesystem for a given mount ID.
/// Iterates the mount registry and returns a clone of the matching Arc.
pub fn fs_by_mount_id(mount_id: usize) -> Option<Arc<dyn super::FileSystem + Send + Sync>> {
    let registry = VFS.read();
    for entry in registry.values() {
        if entry.mount_id == mount_id {
            return Some(entry.fs.clone());
        }
    }
    None
}

/// Register an inode as having dirty MAP_SHARED pages, so the writeback
/// kthread can flush it periodically. Duplicate registrations are deduplicated.
pub fn register_dirty_inode(inode: &Arc<VfsInode>) {
    let weak = Arc::downgrade(inode);
    let mut list = DIRTY_INODES.lock();
    // Deduplicate: if already present, skip.
    let already = list
        .iter()
        .any(|w| w.upgrade().map(|a| Arc::ptr_eq(&a, inode)).unwrap_or(false));
    if !already {
        list.push(weak);
    }
}

/// Flush dirty MAP_SHARED pages for all registered dirty inodes.
/// Called by the writeback kthread on its periodic pass.
/// Removes tombstoned (dropped) entries from the list as a side effect.
pub fn flush_dirty_inodes() {
    // Snapshot live inodes under the lock; flush outside it to avoid blocking
    // the lock during AHCI I/O.
    let live: Vec<Arc<VfsInode>> = {
        let mut list = DIRTY_INODES.lock();
        let live: Vec<Arc<VfsInode>> = list.iter().filter_map(|w| w.upgrade()).collect();
        // Compact tombstoned entries.
        list.retain(|w| w.upgrade().is_some());
        live
    };

    for inode in live {
        let fs = match fs_by_mount_id(inode.mount_id) {
            Some(f) => f,
            None => continue,
        };
        let pc_ops = match fs.as_page_cache_ops() {
            Some(ops) => ops,
            None => continue,
        };
        let ino = inode.ino;
        let _ = inode
            .pages
            .flush_dirty(|page_idx, buf| pc_ops.flush_page(ino, page_idx, buf, 4096));
    }
}

/// D.2 — Truncate invalidation: unmap PTEs past `new_size` in every process
/// that has a FileBacked VMA referencing `inode`.
///
/// # Lock ordering
/// Caller must hold inode.lock (write) before calling this function.
/// Inside we acquire: inode.mappers.lock > vmas.lock > memory_manager.lock.
/// The vmas and mm locks are released before issuing the TLB shootdown.
///
/// # Partial-VMA handling
/// A VMA may straddle the new_size boundary.  Only pages whose file offset
/// is >= new_size are invalidated; pages before new_size are left mapped.
/// The VMA itself is NOT split; only the affected PTE slots are cleared.
pub fn invalidate_mappings_above(inode: &Arc<VfsInode>, new_size: u64) {
    use crate::thread::UserThread;

    // Collect live UserThread Arcs while holding the mappers lock, then
    // drop the lock before doing any MM work.
    let live: Vec<alloc::sync::Arc<spin::RwLock<UserThread>>> = {
        let mut mappers = inode.mappers.lock();
        // Compact tombstoned entries as a side effect.
        mappers.retain(|w| w.upgrade().is_some());
        mappers.iter().filter_map(|w| w.upgrade()).collect()
    };

    // Accumulate (start_virt, page_count) pairs for the TLB shootdown after
    // all per-process unmaps are done (cannot hold mm lock during shootdown).
    let mut shootdown_ranges: Vec<(VirtAddr, u64)> = Vec::new();

    for user_arc in &live {
        let user = user_arc.read();
        let mut vmas = user.vmas.lock();
        let mut mm = user.memory_manager.lock();

        // Walk all VMAs in this process looking for FileBacked on this inode.
        // Collect the list of (vma_start, slots_to_invalidate) first, then
        // process them, because we need mutable access to each VMA.
        let vma_starts: Vec<VirtAddr> = vmas
            .iter()
            .filter_map(|vma| match &vma.backing {
                VmaBacking::FileBacked { inode: vi, .. } if Arc::ptr_eq(vi, inode) => {
                    Some(vma.start)
                }
                _ => None,
            })
            .collect();

        for vma_start in vma_starts {
            let vma = match vmas.find_mut(vma_start) {
                Some(v) => v,
                None => continue,
            };

            let (file_offset, pages) = match &mut vma.backing {
                VmaBacking::FileBacked {
                    file_offset, pages, ..
                } => (*file_offset, pages),
                _ => continue,
            };

            // The file byte range covered by this VMA is [file_offset, file_offset + vma.size()).
            // Pages whose file offset >= new_size must be invalidated.
            let vma_size = vma.end.as_u64() - vma.start.as_u64();
            let num_slots = (vma_size / 4096) as usize;

            // First slot index (within the VMA) that is past new_size.
            let first_invalid_slot = if new_size <= file_offset {
                // The entire VMA is past new_size.
                0usize
            } else {
                let bytes_before_cut = new_size - file_offset;
                ((bytes_before_cut + 4095) / 4096) as usize
            };

            if first_invalid_slot >= num_slots {
                // No slots past new_size in this VMA.
                continue;
            }

            let mut invalidated_pages: u64 = 0;
            let first_invalid_virt =
                VirtAddr::new(vma.start.as_u64() + first_invalid_slot as u64 * 4096);

            for slot in first_invalid_slot..num_slots {
                let virt = VirtAddr::new(vma.start.as_u64() + slot as u64 * 4096);
                let page: Page<Size4KiB> = Page::containing_address(virt);

                if let Ok(phys) = mm.mapper.translate_page(page) {
                    if let Ok((_, flush)) = mm.mapper.unmap(page) {
                        flush.ignore();
                        // Decrement the refcount bumped at fault-in time.
                        frame_allocator().dec_refcount(phys);
                        invalidated_pages += 1;
                    }
                }
                // Drop the Arc<CachedPage> for this slot; the inode's page
                // cache entry will be freed by invalidate_from() after we return.
                if slot < pages.len() {
                    pages[slot] = None;
                }
            }

            if invalidated_pages > 0 {
                shootdown_ranges.push((first_invalid_virt, invalidated_pages));
            }
        }
        // vmas lock and mm lock drop here.
    }

    // Issue TLB shootdowns after all MM locks are released.
    for (start, count) in shootdown_ranges {
        if crate::memory::tlb::shootdown_needed() {
            crate::memory::tlb::tlb_shootdown(start, count);
        }
    }
}

/// Fetch or fill a single page from an inode's page cache.
///
/// Returns an `Arc<CachedPage>` that callers can clone to keep the page alive
/// independent of the `PageGuard` drop guard.  The returned Arc has one
/// logical pin contributed by the caller; callers must call `page.unpin()`
/// when they no longer need the page held (e.g. on munmap).
///
/// Uses `PageCacheOps::fill_page` supplied by `fs` (the inode's filesystem,
/// passed in by the caller to avoid a redundant `fs_by_mount_id` lookup when
/// the caller has already resolved it).
///
/// Returns `Err(Errno::EINVAL)` if the filesystem does not support the page cache
/// or the inode number is zero.
pub fn get_or_fill_page(
    inode: &Arc<super::inode::VfsInode>,
    page_idx: u64,
    fs: &Arc<dyn super::FileSystem + Send + Sync>,
) -> Result<Arc<super::page_cache::CachedPage>, super::super::syscalls::Errno> {
    let pc_ops = fs
        .as_page_cache_ops()
        .ok_or(super::super::syscalls::Errno::EINVAL)?;
    let ino = inode.ino;
    let guard = inode
        .pages
        .get_or_fill(page_idx, |buf| {
            let valid = pc_ops.fill_page(ino, page_idx, buf)?;
            if valid < 4096 {
                buf[valid..].fill(0);
            }
            Ok(())
        })
        .map_err(|_| super::super::syscalls::Errno::EIO)?;
    // Extract the Arc before the guard drops (and unpins).
    // Pin explicitly so the caller holds a pin independent of the guard.
    let page = guard.arc();
    page.pin();
    Ok(page)
}
