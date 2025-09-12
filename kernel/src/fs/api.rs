// Public api methods to send requests transparently
#![expect(unused)]

use core::time::Duration;

use alloc::{collections::btree_map::BTreeMap, vec::Vec};

use crate::{
    fs::{
        Error, FS_REQUESTS, File, FsRequest, FsResponse, PartitionCommand, gpt::Partition,
        path::Path,
    },
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
    let FsResponse::Partitions(parts) =
        send_request(FsRequest::ListPartitions, Duration::from_secs(1))
    else {
        unreachable!()
    };
    parts
}

pub fn list_mounts() -> BTreeMap<Path, usize> {
    let FsResponse::MountPoints(mp) = send_request(FsRequest::ListMounts, Duration::from_secs(1))
    else {
        unreachable!()
    };
    mp
}

pub fn mount_partition(index: usize, mount_point: Path) -> Result<(), Error> {
    let FsResponse::Ok(result) = send_request(
        FsRequest::Mount { index, mount_point },
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

// Partition-scoped APIs (index refers to the partition position returned by list_partitions())

pub fn list_files(index: usize, path: &Path) -> Result<Vec<File>, Error> {
    let cmd = PartitionCommand::ListFiles { path: path.clone() };
    let FsResponse::Files(r) = send_request(
        FsRequest::PartitionRequest {
            index,
            command: cmd,
        },
        Duration::from_secs(5),
    ) else {
        unreachable!()
    };
    r
}

pub fn read_bytes(
    index: usize,
    path: &Path,
    offset: usize,
    count: usize,
) -> Result<Vec<u8>, Error> {
    let cmd = PartitionCommand::ReadBytes {
        path: path.clone(),
        offset,
        count,
    };
    let FsResponse::ReadBytes(r) = send_request(
        FsRequest::PartitionRequest {
            index,
            command: cmd,
        },
        Duration::from_secs(10),
    ) else {
        unreachable!()
    };
    r
}

pub fn write_bytes(index: usize, path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
    let cmd = PartitionCommand::WriteBytes {
        path: path.clone(),
        offset,
        data: data.to_vec(),
    };
    let FsResponse::Written(r) = send_request(
        FsRequest::PartitionRequest {
            index,
            command: cmd,
        },
        Duration::from_secs(10),
    ) else {
        unreachable!()
    };
    r
}

pub fn create_file(index: usize, path: &Path) -> Result<(), Error> {
    let cmd = PartitionCommand::CreateFile { path: path.clone() };
    let FsResponse::Ok(r) = send_request(
        FsRequest::PartitionRequest {
            index,
            command: cmd,
        },
        Duration::from_secs(5),
    ) else {
        unreachable!()
    };
    r
}

pub fn create_dir(index: usize, path: &Path) -> Result<(), Error> {
    let cmd = PartitionCommand::CreateDir { path: path.clone() };
    let FsResponse::Ok(r) = send_request(
        FsRequest::PartitionRequest {
            index,
            command: cmd,
        },
        Duration::from_secs(5),
    ) else {
        unreachable!()
    };
    r
}

pub fn remove_file(index: usize, path: &Path) -> Result<(), Error> {
    let cmd = PartitionCommand::RemoveFile { path: path.clone() };
    let FsResponse::Ok(r) = send_request(
        FsRequest::PartitionRequest {
            index,
            command: cmd,
        },
        Duration::from_secs(5),
    ) else {
        unreachable!()
    };
    r
}

pub fn remove_dir(index: usize, path: &Path) -> Result<(), Error> {
    let cmd = PartitionCommand::RemoveDir { path: path.clone() };
    let FsResponse::Ok(r) = send_request(
        FsRequest::PartitionRequest {
            index,
            command: cmd,
        },
        Duration::from_secs(5),
    ) else {
        unreachable!()
    };
    r
}

pub fn file_info(index: usize, path: &Path) -> Result<File, Error> {
    let cmd = PartitionCommand::FileInfo { path: path.clone() };
    let FsResponse::File(r) = send_request(
        FsRequest::PartitionRequest {
            index,
            command: cmd,
        },
        Duration::from_secs(5),
    ) else {
        unreachable!()
    };
    r
}

pub fn flush(index: usize) -> Result<(), Error> {
    let cmd = PartitionCommand::Flush;
    let FsResponse::Ok(r) = send_request(
        FsRequest::PartitionRequest {
            index,
            command: cmd,
        },
        Duration::from_secs(5),
    ) else {
        unreachable!()
    };
    r
}
