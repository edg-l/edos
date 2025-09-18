// Public api methods to send requests transparently
#![expect(unused)]

use core::time::Duration;

use alloc::{collections::btree_map::BTreeMap, vec::Vec};

use crate::{
    fs::{Error, FS_REQUESTS, File, FsRequest, FsResponse, PathOp, gpt::Partition, path::Path},
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

pub fn list_mounts() -> BTreeMap<Path, (usize, usize)> {
    let FsResponse::MountPoints(mp) = send_request(FsRequest::ListMounts, Duration::from_secs(1))
    else {
        unreachable!()
    };
    mp
}

pub fn mount_partition(
    device_id: usize,
    partition_index: usize,
    mount_point: Path,
) -> Result<(), Error> {
    let FsResponse::Ok(result) = send_request(
        FsRequest::Mount {
            device_id,
            partition_index,
            mount_point,
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
        Duration::from_secs(10),
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
        Duration::from_secs(10),
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
        Duration::from_secs(5),
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
        Duration::from_secs(5),
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
        Duration::from_secs(5),
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
        Duration::from_secs(5),
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
        Duration::from_secs(5),
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
        Duration::from_secs(5),
    ) else {
        return Err(Error::IoError);
    };
    r
}
