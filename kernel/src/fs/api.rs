// Public api methods to send requests transparently

use alloc::{boxed::Box, string::ToString, sync::Arc, vec::Vec};
use spin::Mutex;

use crate::{
    fs::{
        Error, FS_REQUESTS, File, FileAttrs, FileKind, FsRequest, FsResponse, MmapRegion,
        MountInfo,
        dentry::dentry_cache,
        gpt::{FilesystemType, Partition},
        handle::Pollable,
        inode::VfsInode,
        path::Path,
        vfs::{self, VfsLookup},
    },
    memory::mapper::MemoryManager,
};

pub(super) fn send_request(request: FsRequest) -> FsResponse {
    let requests = FS_REQUESTS.wait();
    let response = requests.send(request);
    response.wait()
}

/// Resolve a path to its VfsInode, using the dentry cache.
/// On cache miss, calls the filesystem's resolve_inode + file_info to populate.
pub fn resolve_vfs_inode(lk: &VfsLookup) -> Option<Arc<VfsInode>> {
    let dc = dentry_cache();

    // Fast path: dentry cache hit.
    if let Some(inode) = dc.lookup(lk.mount_id, &lk.relative) {
        return Some(inode);
    }

    // Slow path: ask filesystem for inode number and metadata.
    let ino = lk.fs.resolve_inode(&lk.relative).ok()?;
    let info = lk.fs.file_info(&lk.relative).ok()?;
    let inode = VfsInode::new(lk.mount_id, ino, info.kind, info.size);
    dc.insert(lk.mount_id, lk.relative.clone(), inode.clone());
    Some(inode)
}

// Global/management APIs

pub fn list_partitions() -> Vec<Partition> {
    let res = send_request(FsRequest::ListPartitions);
    let FsResponse::Partitions(parts) = res else {
        unreachable!("{:#?}", res)
    };
    parts
}

pub fn list_mounts() -> Vec<MountInfo> {
    vfs::list_mounts()
}

/// If the filesystem is backed by a device, ensure device_id and partition_index are valid.
///
/// Otherwise they are ignored.
pub fn mount_partition(
    device_id: usize,
    partition_index: usize,
    mount_point: Path,
    fs_type: FilesystemType,
) -> Result<(), Error> {
    let FsResponse::Ok(result) = send_request(FsRequest::Mount {
        device_id,
        partition_index,
        mount_point,
        fstype: fs_type,
    }) else {
        unreachable!()
    };
    result
}

#[expect(unused)]
pub fn unmount(mount_point: Path) -> Result<(), Error> {
    let FsResponse::Ok(result) = send_request(FsRequest::Unmount { mount_point }) else {
        unreachable!()
    };
    result
}

/// Register a partition dynamically (e.g. a USB storage device discovered after boot).
pub fn register_partition(partition: Partition) -> Result<(), Error> {
    let FsResponse::Ok(result) = send_request(FsRequest::RegisterPartition { partition }) else {
        unreachable!()
    };
    result
}

// Path-scoped APIs (resolve filesystem via VFS)
// Read operations acquire per-inode read locks.
// Write operations acquire per-inode write locks.
// Two threads reading different files never block each other.

pub fn list_files(path: &Path) -> Result<Vec<File>, Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    // Acquire per-inode read lock on the directory.
    let inode = resolve_vfs_inode(&lk);
    let _guard = inode.as_ref().map(|i| i.lock.read());

    let mut files = lk.fs.list_files(&lk.relative)?;

    // Append synthetic directory entries for child mount points.
    for (name, _mount_path) in vfs::child_mount_points(path) {
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

pub fn read_bytes(path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    // Per-inode read lock: concurrent reads on different files don't block.
    let inode = resolve_vfs_inode(&lk);
    let _guard = inode.as_ref().map(|i| i.lock.read());

    lk.fs.read_bytes(&lk.relative, offset, count)
}

#[expect(unused)]
pub fn write_bytes(path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    // Per-inode write lock: exclusive access to this file.
    let inode = resolve_vfs_inode(&lk);
    let _guard = inode.as_ref().map(|i| i.lock.write());

    lk.fs.write_bytes(&lk.relative, offset, data)
}

/// Variant that takes ownership of the Vec to avoid an extra to_vec() copy.
pub fn write_bytes_owned(path: &Path, offset: usize, data: Vec<u8>) -> Result<u64, Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    let inode = resolve_vfs_inode(&lk);
    let _guard = inode.as_ref().map(|i| i.lock.write());

    lk.fs.write_bytes(&lk.relative, offset, &data)
}

pub fn create_file(path: &Path) -> Result<(), Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    // Lock the parent directory for mutation.
    let parent = lk.relative.parent();
    let parent_inode = parent.as_ref().and_then(|p| {
        let parent_lk = VfsLookup {
            fs: lk.fs.clone(),
            relative: p.clone(),
            mount_id: lk.mount_id,
        };
        resolve_vfs_inode(&parent_lk)
    });
    let _guard = parent_inode.as_ref().map(|i| i.lock.write());

    let result = lk.fs.create_file(&lk.relative);
    if result.is_ok() {
        dentry_cache().invalidate(lk.mount_id, &lk.relative);
    }
    result
}

pub fn create_dir(path: &Path) -> Result<(), Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    let parent = lk.relative.parent();
    let parent_inode = parent.as_ref().and_then(|p| {
        let parent_lk = VfsLookup {
            fs: lk.fs.clone(),
            relative: p.clone(),
            mount_id: lk.mount_id,
        };
        resolve_vfs_inode(&parent_lk)
    });
    let _guard = parent_inode.as_ref().map(|i| i.lock.write());

    let result = lk.fs.create_dir(&lk.relative);
    if result.is_ok() {
        dentry_cache().invalidate(lk.mount_id, &lk.relative);
    }
    result
}

pub fn remove_file(path: &Path) -> Result<(), Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    let parent = lk.relative.parent();
    let parent_inode = parent.as_ref().and_then(|p| {
        let parent_lk = VfsLookup {
            fs: lk.fs.clone(),
            relative: p.clone(),
            mount_id: lk.mount_id,
        };
        resolve_vfs_inode(&parent_lk)
    });
    let _guard = parent_inode.as_ref().map(|i| i.lock.write());

    let result = lk.fs.remove_file(&lk.relative);
    if result.is_ok() {
        dentry_cache().invalidate(lk.mount_id, &lk.relative);
    }
    result
}

pub fn remove_dir(path: &Path) -> Result<(), Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    let parent = lk.relative.parent();
    let parent_inode = parent.as_ref().and_then(|p| {
        let parent_lk = VfsLookup {
            fs: lk.fs.clone(),
            relative: p.clone(),
            mount_id: lk.mount_id,
        };
        resolve_vfs_inode(&parent_lk)
    });
    let _guard = parent_inode.as_ref().map(|i| i.lock.write());

    let result = lk.fs.remove_dir(&lk.relative);
    if result.is_ok() {
        let dc = dentry_cache();
        dc.invalidate(lk.mount_id, &lk.relative);
        dc.invalidate_children(lk.mount_id, &lk.relative);
    }
    result
}

pub fn file_info(path: &Path) -> Result<File, Error> {
    if vfs::is_mount_point(path) {
        let name = path.last_component().unwrap_or("/").to_string();
        return Ok(File {
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
    let lk = vfs::lookup_for_info(path).ok_or(Error::FileNotFound)?;

    let inode = resolve_vfs_inode(&lk);
    let _guard = inode.as_ref().map(|i| i.lock.read());

    lk.fs.file_info(&lk.relative)
}

#[expect(unused)]
pub fn flush(path: &Path) -> Result<(), Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    lk.fs.flush()
}

pub fn ioctl(path: &Path, request: u64, arg: u64) -> Result<u64, Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    // Treat all ioctls as writes for safety.
    let inode = resolve_vfs_inode(&lk);
    let _guard = inode.as_ref().map(|i| i.lock.write());

    lk.fs.ioctl(&lk.relative, request, arg)
}

pub fn poll(path: &Path) -> Result<Box<dyn Pollable>, Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    lk.fs.poll(&lk.relative)
}

#[expect(unused)]
pub fn mmap(
    path: &Path,
    offset: usize,
    length: usize,
    memory: Arc<Mutex<MemoryManager>>,
) -> Result<MmapRegion, Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    lk.fs.mmap(&lk.relative, offset, length, memory)
}

pub fn truncate(path: &Path, size: u64) -> Result<(), Error> {
    let lk = vfs::lookup(path).ok_or(Error::FileNotFound)?;

    let inode = resolve_vfs_inode(&lk);
    let _guard = inode.as_ref().map(|i| i.lock.write());

    let result = lk.fs.truncate(&lk.relative, size);
    if result.is_ok() {
        dentry_cache().invalidate(lk.mount_id, &lk.relative);
    }
    result
}

pub fn rename(old_path: &Path, new_path: &Path) -> Result<(), Error> {
    let old_lk = vfs::lookup(old_path).ok_or(Error::FileNotFound)?;
    let new_lk = vfs::lookup(new_path).ok_or(Error::FileNotFound)?;

    if !Arc::ptr_eq(&old_lk.fs, &new_lk.fs) {
        return Err(Error::Unsupported);
    }

    // Lock both parent directories for the rename. Use inode number ordering
    // to prevent deadlocks when two renames cross directories.
    let old_parent = old_lk.relative.parent();
    let new_parent = new_lk.relative.parent();

    let old_parent_inode = old_parent.as_ref().and_then(|p| {
        let plk = VfsLookup {
            fs: old_lk.fs.clone(),
            relative: p.clone(),
            mount_id: old_lk.mount_id,
        };
        resolve_vfs_inode(&plk)
    });
    let new_parent_inode = new_parent.as_ref().and_then(|p| {
        let plk = VfsLookup {
            fs: new_lk.fs.clone(),
            relative: p.clone(),
            mount_id: new_lk.mount_id,
        };
        resolve_vfs_inode(&plk)
    });

    // Acquire locks in inode number order to prevent deadlocks.
    let (_guard1, _guard2) = match (&old_parent_inode, &new_parent_inode) {
        (Some(a), Some(b)) if Arc::ptr_eq(a, b) => {
            // Same directory -- single lock.
            (Some(a.lock.write()), None)
        }
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

    let result = old_lk.fs.rename(&old_lk.relative, &new_lk.relative);
    if result.is_ok() {
        let dc = dentry_cache();
        dc.invalidate(old_lk.mount_id, &old_lk.relative);
        dc.invalidate(new_lk.mount_id, &new_lk.relative);
    }
    result
}
