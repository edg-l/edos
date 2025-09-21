// Public api methods to send requests transparently
#![expect(unused)]

use core::time::Duration;

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
    thread::scheduler::sched,
};

pub(super) fn send_request(request: FsRequest, timeout: Duration) -> FsResponse {
    let requests = {
        loop {
            if let Some(req) = FS_REQUESTS.get() {
                break req;
            }
            sched().thread_yield();
        }
    };

    let response = requests.send(request);

    loop {
        match response.receive_timeout(timeout) {
            Ok(res) => break res,
            Err(_) => continue,
        }
    }
}

// Global/management APIs

pub fn list_partitions() -> Vec<Partition> {
    let res = send_request(FsRequest::ListPartitions, Duration::from_secs(1));
    let FsResponse::Partitions(parts) = res else {
        unreachable!("{:#?}", res)
    };
    parts
}

pub fn list_mounts() -> Vec<MountInfo> {
    let FsResponse::Mounts(mounts) = send_request(FsRequest::ListMounts, Duration::from_secs(1))
    else {
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
    let FsResponse::Ok(result) = send_request(
        FsRequest::Mount {
            device_id,
            partition_index,
            mount_point,
            fstype: fs_type,
        },
        Duration::from_secs(1),
    ) else {
        unreachable!()
    };
    result
}

pub fn unmount(mount_point: Path) -> Result<(), Error> {
    let FsResponse::Ok(result) =
        send_request(FsRequest::Unmount { mount_point }, Duration::from_secs(1))
    else {
        unreachable!()
    };
    result
}

// Path-scoped APIs (resolve partition via mount table in FS main)

pub fn list_files(path: &Path) -> Result<Vec<File>, Error> {
    let res = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::ListFiles,
        },
        Duration::from_secs(5),
    );
    let FsResponse::Files(r) = res else {
        return Err(Error::IoError);
    };
    r
}

pub fn read_bytes(path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
    let FsResponse::ReadBytes(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::ReadBytes { offset, count },
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}

pub fn write_bytes(path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
    let FsResponse::Written(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::WriteBytes {
                offset,
                data: data.to_vec(),
            },
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}

pub fn create_file(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::CreateFile,
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}

pub fn create_dir(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::CreateDir,
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}

pub fn remove_file(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::RemoveFile,
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}

pub fn remove_dir(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::RemoveDir,
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}

pub fn file_info(path: &Path) -> Result<File, Error> {
    let FsResponse::File(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::FileInfo,
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}

pub fn flush(path: &Path) -> Result<(), Error> {
    let FsResponse::Ok(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::Flush,
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}

pub fn ioctl(path: &Path, request: u64, arg: u64) -> Result<u64, Error> {
    let FsResponse::Ioctl(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::Ioctl { request, arg },
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}

pub fn poll(path: &Path) -> Result<Box<dyn Pollable>, Error> {
    let FsResponse::Poll(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::Poll,
        },
        Duration::from_secs(1),
    ) else {
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
    let FsResponse::Mmap(r) = send_request(
        FsRequest::PathRequest {
            path: path.clone(),
            op: PathOp::Mmap {
                offset,
                length,
                memory,
            },
        },
        Duration::from_secs(1),
    ) else {
        return Err(Error::IoError);
    };
    r
}
