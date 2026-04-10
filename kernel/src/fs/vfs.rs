use alloc::{
    boxed::Box,
    collections::btree_map::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use spin::RwLock;

use super::{FileSystem, MountInfo, path::Path};
use crate::{fs::gpt::FilesystemType, thread::rwlock::RwLock as BlockingRwLock};

pub struct MountEntry {
    pub fs: Arc<BlockingRwLock<Box<dyn FileSystem + Send + Sync>>>,
    pub device_id: usize,
    pub partition_index: usize,
    pub filesystem: FilesystemType,
}

static VFS: RwLock<BTreeMap<Path, MountEntry>> = RwLock::new(BTreeMap::new());

/// Look up the filesystem for a given path. Returns (filesystem, mount-relative path).
/// Uses longest-prefix matching to find the deepest mount point.
pub fn lookup(
    path: &Path,
) -> Option<(Arc<BlockingRwLock<Box<dyn FileSystem + Send + Sync>>>, Path)> {
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
    Some((entry.fs.clone(), relative))
}

/// Look up for FileInfo specifically - handles the case where the path itself is a mount point
/// by resolving through the parent filesystem.
pub fn lookup_for_info(
    path: &Path,
) -> Option<(Arc<BlockingRwLock<Box<dyn FileSystem + Send + Sync>>>, Path)> {
    if VFS.read().contains_key(path) {
        if let Some(parent) = path.parent() {
            return lookup(&parent).map(|(fs, parent_rel)| {
                let name = path.last_component().unwrap_or("");
                let relative = parent_rel.join(name).normalize();
                (fs, relative)
            });
        }
    }
    lookup(path)
}

pub fn mount(mount_point: Path, entry: MountEntry) {
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
