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
use crate::{fs::gpt::FilesystemType, memory::mapper::MemoryManager};

static NEXT_MOUNT_ID: AtomicUsize = AtomicUsize::new(1);

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
            return page_cache_read(inode, pc_ops, offset, count);
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

    Ok(new_size)
}

pub fn truncate(op: &VfsOp, size: u64) -> Result<(), Error> {
    let _guard = op.inode.as_ref().map(|i| i.lock.write());
    let result = op.fs.truncate(&op.relative, size);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
        if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
            let from_page = (size as usize + 4095) / 4096;
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

pub fn remove_file(op: &VfsOp) -> Result<(), Error> {
    let parent_inode = resolve_parent_inode(op);
    let _guard = parent_inode.as_ref().map(|i| i.lock.write());
    let result = op.fs.remove_file(&op.relative);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
        if let Some(inode) = op.inode.as_ref().filter(|i| i.ino != 0) {
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
