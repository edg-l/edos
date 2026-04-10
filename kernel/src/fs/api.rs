// Public api methods to send requests transparently

use alloc::{boxed::Box, string::ToString, sync::Arc, vec::Vec};
use spin::Mutex;

use crate::{
    fs::{
        Error, FS_REQUESTS, File, FileAttrs, FileKind, FsRequest, FsResponse, MmapRegion,
        MountInfo,
        gpt::{FilesystemType, Partition},
        handle::Pollable,
        path::Path,
        vfs,
    },
    memory::mapper::MemoryManager,
};

pub(super) fn send_request(request: FsRequest) -> FsResponse {
    let requests = FS_REQUESTS.wait();
    let response = requests.send(request);
    response.wait()
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
// Read-only operations use fs.read() (shared lock).
// Write/mutating operations use fs.write() (exclusive lock).

pub fn list_files(path: &Path) -> Result<Vec<File>, Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    let mut files = fs.read().list_files(&rel_path)?;

    // Append synthetic directory entries for child mount points.
    for (name, _mount_path) in vfs::child_mount_points(path) {
        // Only add if not already present (the underlying FS might list the dir).
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
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.read().read_bytes(&rel_path, offset, count)
}

#[expect(unused)]
pub fn write_bytes(path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.write().write_bytes(&rel_path, offset, data)
}

/// Variant that takes ownership of the Vec to avoid an extra to_vec() copy.
pub fn write_bytes_owned(path: &Path, offset: usize, data: Vec<u8>) -> Result<u64, Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.write().write_bytes(&rel_path, offset, &data)
}

pub fn create_file(path: &Path) -> Result<(), Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.write().create_file(&rel_path)
}

pub fn create_dir(path: &Path) -> Result<(), Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.write().create_dir(&rel_path)
}

pub fn remove_file(path: &Path) -> Result<(), Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.write().remove_file(&rel_path)
}

pub fn remove_dir(path: &Path) -> Result<(), Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.write().remove_dir(&rel_path)
}

pub fn file_info(path: &Path) -> Result<File, Error> {
    // If path is a mount point, resolve through the parent filesystem so we
    // get back the directory entry for the mount root itself.
    if vfs::is_mount_point(path) {
        // Return a synthetic directory entry for the mount point root.
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
    let (fs, rel_path) = vfs::lookup_for_info(path).ok_or(Error::FileNotFound)?;
    fs.read().file_info(&rel_path)
}

#[expect(unused)]
pub fn flush(path: &Path) -> Result<(), Error> {
    let (fs, _rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.write().flush()
}

pub fn ioctl(path: &Path, request: u64, arg: u64) -> Result<u64, Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.write().ioctl(&rel_path, request, arg)
}

pub fn poll(path: &Path) -> Result<Box<dyn Pollable>, Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.read().poll(&rel_path)
}

#[expect(unused)]
pub fn mmap(
    path: &Path,
    offset: usize,
    length: usize,
    memory: Arc<Mutex<MemoryManager>>,
) -> Result<MmapRegion, Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.read().mmap(&rel_path, offset, length, memory)
}

pub fn truncate(path: &Path, size: u64) -> Result<(), Error> {
    let (fs, rel_path) = vfs::lookup(path).ok_or(Error::FileNotFound)?;
    fs.write().truncate(&rel_path, size)
}

pub fn rename(old_path: &Path, new_path: &Path) -> Result<(), Error> {
    let (old_fs, old_rel) = vfs::lookup(old_path).ok_or(Error::FileNotFound)?;
    let (new_fs, new_rel) = vfs::lookup(new_path).ok_or(Error::FileNotFound)?;

    // Both paths must resolve to the same filesystem instance (same Arc pointer).
    if !Arc::ptr_eq(&old_fs, &new_fs) {
        return Err(Error::Unsupported);
    }

    old_fs.write().rename(&old_rel, &new_rel)
}
