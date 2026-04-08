// Public api methods to send requests transparently
#![expect(unused)]

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::Mutex;

use crate::{
    fs::{
        Error, FS_REQUESTS, File, FsRequest, FsResponse, MmapRegion, MountInfo, PathOp, PollState,
        gpt::{FilesystemType, Partition},
        handle::Pollable,
        path::Path,
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
    let FsResponse::Mounts(mounts) = send_request(FsRequest::ListMounts) else {
        unreachable!()
    };
    mounts
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

// Path-scoped APIs (resolve partition via mount table in FS main)

pub fn list_files(path: &Path) -> Result<Vec<File>, Error> {
    let res = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::ListFiles,
    });
    let FsResponse::Files(r) = res else {
        return Err(Error::IoError);
    };
    r
}

pub fn read_bytes(path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
    let FsResponse::ReadBytes(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::ReadBytes { offset, count },
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn write_bytes(path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
    let FsResponse::Written(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::WriteBytes {
            offset,
            data: data.to_vec(),
        },
    }) else {
        return Err(Error::IoError);
    };
    r
}

/// Variant that takes ownership of the Vec to avoid an extra to_vec() copy.
pub fn write_bytes_owned(path: &Path, offset: usize, data: Vec<u8>) -> Result<u64, Error> {
    let FsResponse::Written(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::WriteBytes { offset, data },
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn create_file(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::CreateFile,
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn create_dir(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::CreateDir,
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn remove_file(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::RemoveFile,
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn remove_dir(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::RemoveDir,
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn file_info(path: &Path) -> Result<File, Error> {
    let FsResponse::File(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::FileInfo,
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn flush(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::Flush,
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn ioctl(path: &Path, request: u64, arg: u64) -> Result<u64, Error> {
    let FsResponse::Ioctl(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::Ioctl { request, arg },
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn poll(path: &Path) -> Result<Box<dyn Pollable>, Error> {
    let FsResponse::Poll(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::Poll,
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn mmap(
    path: &Path,
    offset: usize,
    length: usize,
    memory: Arc<Mutex<MemoryManager>>,
) -> Result<MmapRegion, Error> {
    let FsResponse::Mmap(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::Mmap {
            offset,
            length,
            memory,
        },
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn truncate(path: &Path, size: u64) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(FsRequest::PathRequest {
        path: path.clone(),
        op: PathOp::Truncate { size },
    }) else {
        return Err(Error::IoError);
    };
    r
}

pub fn rename(old_path: &Path, new_path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(FsRequest::PathRequest {
        path: old_path.clone(),
        op: PathOp::Rename {
            new_path: new_path.clone(),
        },
    }) else {
        return Err(Error::IoError);
    };
    r
}
