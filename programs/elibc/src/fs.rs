use alloc::string::ToString;
use alloc::{string::String, vec, vec::Vec};
use core::mem::{ManuallyDrop, MaybeUninit};
use core::{ffi::CStr, mem::size_of, ptr, str};

use crate::{
    Errno, errno, sys_list_mounts, sys_list_partitions, sys_mkdir, sys_mount, sys_rmdir,
    sys_rmdir_all, sys_unlink,
};

/// Partition information returned by the kernel when listing partitions.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PartitionInfo {
    pub index: usize,
    pub starting_lba: u64,
    pub ending_lba: u64,
    pub size_sectors: u64,
    pub device_id: u64,
    pub unique_partition_guid: [u8; 16],
}

impl PartitionInfo {
    /// Returns the number of bytes occupied by a partition entry as written by the kernel.
    #[inline]
    pub const fn byte_size() -> usize {
        size_of::<PartitionInfo>()
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawMountEntry {
    path_len: u32,
    filesystem: u32,
    device_id: u64,
    partition_index: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemKind {
    Unknown,
    Fat12,
    Fat16,
    Fat32,
    Ntfs,
    Iso9660,
    Memfs,
    Devfs,
}

impl FilesystemKind {
    fn from_u32(value: u32) -> Self {
        match value {
            1 => FilesystemKind::Fat12,
            2 => FilesystemKind::Fat16,
            3 => FilesystemKind::Fat32,
            4 => FilesystemKind::Ntfs,
            5 => FilesystemKind::Iso9660,
            6 => FilesystemKind::Memfs,
            7 => FilesystemKind::Devfs,
            _ => FilesystemKind::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FilesystemKind::Unknown => "unknown",
            FilesystemKind::Fat12 => "fat12",
            FilesystemKind::Fat16 => "fat16",
            FilesystemKind::Fat32 => "fat32",
            FilesystemKind::Ntfs => "ntfs",
            FilesystemKind::Iso9660 => "iso9660",
            FilesystemKind::Memfs => "memfs",
            FilesystemKind::Devfs => "devfs",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MountInfo {
    pub path: String,
    pub filesystem: FilesystemKind,
    pub device_id: u64,
    pub partition_index: u64,
}

/// Mount a partition to a mount point directory.
pub fn mount_partition(
    device_id: u64,
    partition_idx: u64,
    mount_point: &CStr,
    fs_type: &CStr,
) -> Result<(), Errno> {
    let result = unsafe {
        sys_mount(
            device_id,
            partition_idx,
            mount_point.as_ptr().cast(),
            fs_type.as_ptr().cast(),
        )
    };
    if result == 0 { Ok(()) } else { Err(errno()) }
}

/// Create a directory at `path`.
pub fn create_dir(path: &CStr) -> Result<(), Errno> {
    let result = unsafe { sys_mkdir(path.as_ptr().cast()) };
    if result == 0 { Ok(()) } else { Err(errno()) }
}

/// Remove an empty directory at `path`.
pub fn remove_dir(path: &CStr) -> Result<(), Errno> {
    let result = unsafe { sys_rmdir(path.as_ptr().cast()) };
    if result == 0 { Ok(()) } else { Err(errno()) }
}

/// Remove a directory and all of its contents.
pub fn remove_dir_all(path: &CStr) -> Result<(), Errno> {
    let result = unsafe { sys_rmdir_all(path.as_ptr().cast()) };
    if result == 0 { Ok(()) } else { Err(errno()) }
}

/// Remove a file located at `path`.
pub fn remove_file(path: &CStr) -> Result<(), Errno> {
    let result = unsafe { sys_unlink(path.as_ptr().cast()) };
    if result == 0 { Ok(()) } else { Err(errno()) }
}

/// Retrieve all known partitions from the kernel.
pub fn list_partitions() -> Result<Vec<PartitionInfo>, Errno> {
    const INITIAL_CAPACITY: usize = 4;
    let entry_size = PartitionInfo::byte_size();

    let mut capacity = INITIAL_CAPACITY.max(1);
    loop {
        let mut buf: Vec<MaybeUninit<PartitionInfo>> = Vec::with_capacity(capacity);
        let buf_ptr = buf.as_mut_ptr() as *mut u8;
        let buf_size = capacity * entry_size;

        let written = unsafe { sys_list_partitions(buf_ptr, buf_size) };
        if written < 0 {
            return Err(errno());
        }

        let written = written as usize;
        if !written.is_multiple_of(entry_size) {
            return Err(Errno::EIO);
        }

        let count = written / entry_size;
        if count == capacity && written == buf_size {
            capacity = capacity.checked_mul(2).ok_or(Errno::ENOMEM)?;
            continue;
        }

        unsafe {
            buf.set_len(count);
            let mut buf = ManuallyDrop::new(buf);
            let ptr = buf.as_mut_ptr() as *mut PartitionInfo;
            let cap = buf.capacity();
            return Ok(Vec::from_raw_parts(ptr, count, cap));
        }
    }
}

enum MountParseError {
    Truncated,
    Invalid,
}

fn parse_mount_buffer(buffer: &[u8]) -> Result<Vec<MountInfo>, MountParseError> {
    let mut offset = 0usize;
    let mut mounts = Vec::new();
    let entry_size = size_of::<RawMountEntry>();

    while offset < buffer.len() {
        if buffer.len() - offset < entry_size {
            return Err(MountParseError::Truncated);
        }

        let entry =
            unsafe { ptr::read_unaligned(buffer[offset..].as_ptr() as *const RawMountEntry) };
        offset += entry_size;

        let path_len = entry.path_len as usize;
        if buffer.len() - offset < path_len {
            return Err(MountParseError::Truncated);
        }

        let path_bytes = &buffer[offset..offset + path_len];
        offset += path_len;

        let path = str::from_utf8(path_bytes)
            .map_err(|_| MountParseError::Invalid)?
            .to_string();

        mounts.push(MountInfo {
            path,
            filesystem: FilesystemKind::from_u32(entry.filesystem),
            device_id: entry.device_id,
            partition_index: entry.partition_index,
        });
    }

    Ok(mounts)
}

/// Retrieve the list of mounted filesystems from the kernel.
pub fn list_mounts() -> Result<Vec<MountInfo>, Errno> {
    let mut capacity = 4096usize;

    loop {
        let mut buffer = vec![0u8; capacity];
        let written = unsafe { sys_list_mounts(buffer.as_mut_ptr(), buffer.len()) };

        if written < 0 {
            return Err(errno());
        }

        let written = written as usize;
        let slice = &buffer[..written];

        match parse_mount_buffer(slice) {
            Ok(mounts) => return Ok(mounts),
            Err(MountParseError::Truncated) => {
                capacity = capacity.checked_mul(2).ok_or(Errno::ENOMEM)?;
                continue;
            }
            Err(MountParseError::Invalid) => return Err(Errno::EIO),
        }
    }
}
