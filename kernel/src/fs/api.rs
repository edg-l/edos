// Public api methods to send requests transparently

use crate::thread::preempt::PreemptSpinlock;
use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use crate::{
    fs::{
        Error, FS_REQUESTS, File, FileAttrs, FileKind, FsRequest, FsResponse, LinkMode,
        MAX_SYMLINK_HOPS, MmapRegion, MountInfo,
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
        FsResponse::Partitions(_) | FsResponse::Count(_) => Err(Error::ProtocolMismatch),
    }
}

fn expect_partitions(res: FsResponse) -> Result<Vec<Partition>, Error> {
    match res {
        FsResponse::Partitions(parts) => Ok(parts),
        FsResponse::Ok(_) | FsResponse::Count(_) => Err(Error::ProtocolMismatch),
    }
}

fn expect_count(res: FsResponse) -> Result<usize, Error> {
    match res {
        FsResponse::Count(result) => result,
        FsResponse::Ok(_) | FsResponse::Partitions(_) => Err(Error::ProtocolMismatch),
    }
}

pub fn list_partitions() -> Result<Vec<Partition>, Error> {
    expect_partitions(send_request(FsRequest::ListPartitions))
}

/// Re-read `device_id`'s partition table, replacing whatever was known about
/// it. Fails with `Busy` if the device backs a mounted filesystem. Returns the
/// number of partitions now known for the device.
pub fn rescan_partitions(device_id: u64) -> Result<usize, Error> {
    expect_count(send_request(FsRequest::RescanPartitions { device_id }))
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

/// How a path API turns its path into a `VfsOp`.
#[derive(Clone, Copy)]
enum Resolver {
    /// Full resolution, with the inode from the dentry cache.
    Inode,
    /// The mount only, for operations whose target need not exist yet.
    Mount,
    /// Full resolution that also handles the path naming a mount point.
    Info,
}

impl Resolver {
    fn resolve(self, path: &Path) -> Option<vfs::VfsOp> {
        match self {
            Resolver::Inode => vfs::resolve(path),
            Resolver::Mount => vfs::resolve_mount(path),
            Resolver::Info => vfs::resolve_for_info(path),
        }
    }
}

/// Run a path operation, restarting it when a symbolic link on the path names
/// something outside the mount the link lives on.
///
/// A filesystem resolves paths from its own root and cannot see the mount
/// table, so it stops at such a link and reports `Error::LinkEscape`; the VFS
/// says where the target lands and the operation runs again there, possibly on
/// another filesystem. A path with no links, or only links that stay inside
/// their mount, costs exactly one walk.
///
/// The path the operation finally ran on is returned with its result, since
/// `open` caches it for the life of the file descriptor.
fn with_links<T>(
    path: &Path,
    resolver: Resolver,
    mode: LinkMode,
    mut f: impl FnMut(&vfs::VfsOp, &Path) -> Result<T, Error>,
) -> Result<(T, Path), Error> {
    let mut path = path.clone();
    for _ in 0..MAX_SYMLINK_HOPS {
        let op = resolver.resolve(&path).ok_or(Error::FileNotFound)?;
        match f(&op, &path) {
            Err(Error::LinkEscape) => path = vfs::link_escape(&op, mode)?,
            result => return result.map(|value| (value, path)),
        }
    }
    Err(Error::TooManyLinks)
}

/// `with_links` for the callers that do not need the resolved path back.
fn on_path<T>(
    path: &Path,
    resolver: Resolver,
    mode: LinkMode,
    f: impl FnMut(&vfs::VfsOp, &Path) -> Result<T, Error>,
) -> Result<T, Error> {
    with_links(path, resolver, mode, f).map(|(value, _)| value)
}

/// Follow the escaping symbolic links on `path` without acting on what it
/// names, for the caller that cannot use `with_links` because it holds two
/// paths at once.
///
/// The probe has to walk the path the same way the operation will, or it would
/// report an escape the operation does not hit, or miss one it does:
/// `file_info` follows the final component and `read_link` leaves it alone,
/// which is exactly the distinction `mode` draws. Neither's own result is of
/// any interest here; only whether the walk escaped the mount.
fn resolve_links(path: &Path, mode: LinkMode) -> Result<Path, Error> {
    let mut path = path.clone();
    for _ in 0..MAX_SYMLINK_HOPS {
        let op = vfs::resolve_mount(&path).ok_or(Error::FileNotFound)?;
        let probe = match mode {
            LinkMode::Follow => vfs::file_info(&op).map(|_| ()),
            LinkMode::NoFollow => vfs::read_link(&op).map(|_| ()),
        };
        match probe {
            Err(Error::LinkEscape) => path = vfs::link_escape(&op, mode)?,
            _ => return Ok(path),
        }
    }
    Err(Error::TooManyLinks)
}

pub fn list_files(path: &Path) -> Result<Vec<File>, Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, resolved| {
        vfs::list_files(op, resolved)
    })
}

pub fn read_bytes(path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        // Path-API reads have no fd, so readahead state is not preserved across calls.
        let mut ra = ReadaheadState::default();
        vfs::read(op, &mut ra, offset, count)
    })
}

#[expect(unused)]
pub fn write_bytes(path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        vfs::write(op, offset, data, false)
    })
}

#[expect(unused)]
pub fn write_bytes_owned(path: &Path, offset: usize, data: Vec<u8>) -> Result<u64, Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        vfs::write(op, offset, &data, false)
    })
}

pub fn create_file(path: &Path) -> Result<(), Error> {
    on_path(path, Resolver::Mount, LinkMode::NoFollow, |op, _| {
        vfs::create_file(op)
    })
}

/// Create a symbolic link at `path` holding `target` verbatim. The target is
/// not resolved or validated: a dangling link is legal, as in POSIX.
pub fn symlink(target: &str, path: &Path) -> Result<(), Error> {
    on_path(path, Resolver::Mount, LinkMode::NoFollow, |op, _| {
        vfs::symlink(op, target)
    })
}

/// Read the target of the symbolic link at `path` without following it.
pub fn read_link(path: &Path) -> Result<String, Error> {
    on_path(path, Resolver::Mount, LinkMode::NoFollow, |op, _| {
        vfs::read_link(op)
    })
}

pub fn create_dir(path: &Path) -> Result<(), Error> {
    on_path(path, Resolver::Mount, LinkMode::NoFollow, |op, _| {
        vfs::create_dir(op)
    })
}

pub fn remove_file(path: &Path) -> Result<(), Error> {
    on_path(path, Resolver::Inode, LinkMode::NoFollow, |op, _| {
        vfs::remove_file(op)
    })
}

pub fn remove_dir(path: &Path) -> Result<(), Error> {
    on_path(path, Resolver::Inode, LinkMode::NoFollow, |op, _| {
        vfs::remove_dir(op)
    })
}

pub fn file_info(path: &Path) -> Result<File, Error> {
    file_info_resolved(path).map(|(info, _)| info)
}

/// `file_info` that also hands back the path it resolved to, which differs
/// from `path` exactly when a symbolic link redirected it across a mount.
/// `open` needs it: everything it caches on the file descriptor has to name
/// the file the fd actually refers to.
pub fn file_info_resolved(path: &Path) -> Result<(File, Path), Error> {
    if vfs::is_mount_point(path) {
        let name = path.last_component().unwrap_or("/").to_string();
        let info = File {
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
        };
        return Ok((info, path.clone()));
    }
    with_links(path, Resolver::Info, LinkMode::Follow, |op, _| {
        vfs::file_info(op)
    })
}

#[expect(unused)]
pub fn flush(path: &Path) -> Result<(), Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        vfs::flush(op)
    })
}

pub fn flush_file(path: &Path, inode: Option<Arc<VfsInode>>) -> Result<(), Error> {
    let op = vfs::resolve_with_inode(path, inode).ok_or(Error::FileNotFound)?;
    vfs::flush_file(&op)
}

pub fn ioctl(path: &Path, request: u64, arg: u64) -> Result<u64, Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        vfs::ioctl(op, request, arg)
    })
}

pub fn poll(path: &Path) -> Result<Box<dyn Pollable>, Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        vfs::poll(op)
    })
}

#[expect(unused)]
pub fn mmap(
    path: &Path,
    offset: usize,
    length: usize,
    memory: Arc<PreemptSpinlock<MemoryManager>>,
) -> Result<MmapRegion, Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        vfs::mmap(op, offset, length, memory.clone())
    })
}

pub fn truncate(path: &Path, size: u64) -> Result<(), Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        vfs::truncate(op, size)
    })
}

pub fn set_times(path: &Path, atime: Option<u64>, mtime: Option<u64>) -> Result<(), Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        vfs::set_times(op, atime, mtime)
    })
}

/// Rename is the one path operation that takes two paths, so the escapes on
/// each have to be settled before the call rather than by retrying it: a
/// failure would not say which side raised it. Both keep their final component
/// unfollowed, as POSIX requires; a link in the leading components can still
/// redirect either side onto another mount, which `vfs::rename` then refuses,
/// since no filesystem can move a file into a different one.
pub fn rename(old_path: &Path, new_path: &Path) -> Result<(), Error> {
    let old_path = resolve_links(old_path, LinkMode::NoFollow)?;
    let new_path = resolve_links(new_path, LinkMode::NoFollow)?;
    let old_op = vfs::resolve(&old_path).ok_or(Error::FileNotFound)?;
    let new_op = vfs::resolve(&new_path).ok_or(Error::FileNotFound)?;
    vfs::rename(&old_op, &new_op)
}

/// Resolve a path to its VfsInode, returning `Err(Error::FileNotFound)` when
/// the path does not exist or resolves to a directory with no backing inode.
///
/// This is how the ELF loader reaches a binary, so it has to follow symbolic
/// links like every other path API: `spawn`ing `/bin/ll -> /bin/ls` resolves
/// the target's inode. `VfsOp::inode` alone cannot say why it is empty --
/// `resolve` leaves it empty both for a path that does not exist and for one
/// whose walk escaped its mount -- so the operation asks for the file's
/// metadata, whose error carries that distinction back to the retry loop.
pub fn resolve_inode(path: &Path) -> Result<Arc<VfsInode>, Error> {
    on_path(path, Resolver::Inode, LinkMode::Follow, |op, _| {
        if let Some(inode) = op.inode.clone() {
            return Ok(inode);
        }
        vfs::file_info(op)?;
        // The path is there; it just has no inode of its own, as a directory
        // on some filesystems does not.
        Err(Error::FileNotFound)
    })
}
