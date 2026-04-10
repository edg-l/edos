use alloc::{
    boxed::Box,
    collections::btree_map::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::RwLock;

use super::{FileSystem, MountInfo, path::Path};
use crate::fs::gpt::FilesystemType;

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
pub struct VfsLookup {
    pub fs: Arc<dyn FileSystem + Send + Sync>,
    pub relative: Path,
    pub mount_id: usize,
}

/// Look up the filesystem for a given path.
/// Uses longest-prefix matching to find the deepest mount point.
pub fn lookup(path: &Path) -> Option<VfsLookup> {
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
pub fn lookup_for_info(path: &Path) -> Option<VfsLookup> {
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
