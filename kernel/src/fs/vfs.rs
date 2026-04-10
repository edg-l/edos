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
    let inode = VfsInode::new(mount_id, ino, info.kind, info.size);
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
    op.fs.read_bytes(&op.relative, offset, count)
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
        op.fs
            .file_info(&op.relative)
            .map(|f| f.size as usize)
            .unwrap_or(offset)
    } else {
        offset
    };
    op.fs.write_bytes(&op.relative, actual_offset, data)
}

pub fn truncate(op: &VfsOp, size: u64) -> Result<(), Error> {
    let _guard = op.inode.as_ref().map(|i| i.lock.write());
    let result = op.fs.truncate(&op.relative, size);
    if result.is_ok() {
        dentry::dentry_cache().invalidate(op.mount_id, &op.relative);
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
        dc.invalidate(old_op.mount_id, &old_op.relative);
        dc.invalidate(new_op.mount_id, &new_op.relative);
    }
    result
}

// --- Passthrough operations (no locking needed) ---

pub fn flush(op: &VfsOp) -> Result<(), Error> {
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
