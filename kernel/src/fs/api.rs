// Public api methods to send requests transparently

use crate::thread::preempt::PreemptSpinlock;
use alloc::{boxed::Box, string::ToString, sync::Arc, vec::Vec};

use crate::{
    fs::{
        Error, FS_REQUESTS, File, FileAttrs, FileKind, FsRequest, FsResponse, MmapRegion,
        MountInfo,
        gpt::{FilesystemType, Partition},
        handle::Pollable,
        inode::VfsInode,
        path::Path,
        readahead::ReadaheadState,
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

/// The FS thread answers `ListPartitions` with `Partitions` and every other
/// request with `Ok`. These two helpers are the only places that pairing is
/// asserted, so a request variant added later cannot quietly reuse the wrong
/// reply, and a mismatch is an error rather than a kernel panic.
fn expect_ok(res: FsResponse) -> Result<(), Error> {
    match res {
        FsResponse::Ok(result) => result,
        FsResponse::Partitions(_) => Err(Error::ProtocolMismatch),
    }
}

fn expect_partitions(res: FsResponse) -> Result<Vec<Partition>, Error> {
    match res {
        FsResponse::Partitions(parts) => Ok(parts),
        FsResponse::Ok(_) => Err(Error::ProtocolMismatch),
    }
}

pub fn list_partitions() -> Result<Vec<Partition>, Error> {
    expect_partitions(send_request(FsRequest::ListPartitions))
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
    expect_ok(send_request(FsRequest::Mount {
        device_id,
        partition_index,
        mount_point,
        fstype: fs_type,
    }))
}

#[expect(unused)]
pub fn unmount(mount_point: Path) -> Result<(), Error> {
    expect_ok(send_request(FsRequest::Unmount { mount_point }))
}

/// Register a partition dynamically (e.g. a USB storage device discovered after boot).
pub fn register_partition(partition: Partition) -> Result<(), Error> {
    expect_ok(send_request(FsRequest::RegisterPartition { partition }))
}

// Path-scoped APIs (resolve filesystem via VFS)
// Read operations acquire per-inode read locks.
// Write operations acquire per-inode write locks.
// Two threads reading different files never block each other.

pub fn list_files(path: &Path) -> Result<Vec<File>, Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::list_files(&op, path)
}

pub fn read_bytes(path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    // Path-API reads have no fd, so readahead state is not preserved across calls.
    let mut ra = ReadaheadState::default();
    vfs::read(&op, &mut ra, offset, count)
}

#[expect(unused)]
pub fn write_bytes(path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::write(&op, offset, data, false)
}

#[expect(unused)]
pub fn write_bytes_owned(path: &Path, offset: usize, data: Vec<u8>) -> Result<u64, Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::write(&op, offset, &data, false)
}

pub fn create_file(path: &Path) -> Result<(), Error> {
    let op = vfs::resolve_mount(path).ok_or(Error::FileNotFound)?;
    vfs::create_file(&op)
}

pub fn create_dir(path: &Path) -> Result<(), Error> {
    let op = vfs::resolve_mount(path).ok_or(Error::FileNotFound)?;
    vfs::create_dir(&op)
}

pub fn remove_file(path: &Path) -> Result<(), Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::remove_file(&op)
}

pub fn remove_dir(path: &Path) -> Result<(), Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::remove_dir(&op)
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
    let op = vfs::resolve_for_info(path).ok_or(Error::FileNotFound)?;
    vfs::file_info(&op)
}

#[expect(unused)]
pub fn flush(path: &Path) -> Result<(), Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::flush(&op)
}

pub fn flush_file(path: &Path, inode: Option<Arc<VfsInode>>) -> Result<(), Error> {
    let op = vfs::resolve_with_inode(path, inode).ok_or(Error::FileNotFound)?;
    vfs::flush_file(&op)
}

pub fn ioctl(path: &Path, request: u64, arg: u64) -> Result<u64, Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::ioctl(&op, request, arg)
}

pub fn poll(path: &Path) -> Result<Box<dyn Pollable>, Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::poll(&op)
}

#[expect(unused)]
pub fn mmap(
    path: &Path,
    offset: usize,
    length: usize,
    memory: Arc<PreemptSpinlock<MemoryManager>>,
) -> Result<MmapRegion, Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::mmap(&op, offset, length, memory)
}

pub fn truncate(path: &Path, size: u64) -> Result<(), Error> {
    let op = vfs::resolve(path).ok_or(Error::FileNotFound)?;
    vfs::truncate(&op, size)
}

pub fn rename(old_path: &Path, new_path: &Path) -> Result<(), Error> {
    let old_op = vfs::resolve(old_path).ok_or(Error::FileNotFound)?;
    let new_op = vfs::resolve(new_path).ok_or(Error::FileNotFound)?;
    vfs::rename(&old_op, &new_op)
}

/// Resolve a VfsInode for a path (used by sys_open to cache in FsFile).
pub fn resolve_vfs_inode_for_path(path: &Path) -> Option<Arc<VfsInode>> {
    vfs::resolve(path).and_then(|op| op.inode)
}

/// Resolve a path to its VfsInode, returning `Err(Error::FileNotFound)` when
/// the path does not exist or resolves to a directory with no backing inode.
#[allow(dead_code)]
pub fn resolve_inode(path: &Path) -> Result<Arc<VfsInode>, Error> {
    vfs::resolve(path)
        .and_then(|op| op.inode)
        .ok_or(Error::FileNotFound)
}
