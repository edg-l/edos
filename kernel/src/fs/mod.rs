#![expect(unused)]

use core::ffi::CStr;

use alloc::{boxed::Box, format, string::String, vec::Vec};
use spin::Once;
use thiserror::Error;

use crate::{
    allocator::print_alloc_stats,
    drivers::ahci::{AhciError, api::list_devices},
    fs::{
        fat32::Fat32fs,
        gpt::{FilesystemType, Partition, parse_gpt, print_partitions},
        path::Path,
    },
    log,
    thread::{
        ThreadId,
        mailbox::Mailbox,
        scheduler::sched,
        util::{kthread_exit, queue_spawn_kthread_named, queue_spawn_kthread_named_arg},
    },
};

pub mod api;
pub mod block_device;
pub mod fat32;
pub mod gpt;
pub mod path;

pub fn init() {
    queue_spawn_kthread_named("fs", fs_main_thread as u64);
}

#[derive(Debug, Error, Clone)]
pub enum Error {
    #[error("file not found")]
    FileNotFound,
    #[error("not a file")]
    NotAFile,
    #[error("not a directory")]
    NotADir,
    #[error("i/o error")]
    IoError,
    #[error("missing critical sectors, like basic fs info")]
    MissingCriticalSectors,
    #[error(transparent)]
    AhciError(#[from] AhciError),
    #[error("Invalid filesystem, mismatch in verification.")]
    InvalidFs,
    #[error("corrupted filesystem")]
    Corrupted,
}

pub trait FileSystem {
    fn list_files(&self, path: &Path) -> Result<Vec<File>, Error>;

    fn read_bytes(&self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error>;

    fn write_bytes(&mut self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error>;

    fn create_file(&mut self, path: &Path) -> Result<(), Error>;
    fn create_dir(&mut self, path: &Path) -> Result<(), Error>;
    fn remove_dir(&mut self, path: &Path) -> Result<(), Error>;
    fn remove_file(&mut self, path: &Path) -> Result<(), Error>;

    fn file_info(&self, path: &Path) -> Result<File, Error>;

    fn flush(&mut self) -> Result<(), Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Special,
}

#[derive(Debug, Clone, Copy)]
pub struct FileAttrs {
    pub readonly: bool,
    pub hidden: bool,
    pub system: bool,
    pub archive: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct FileTime {
    pub date: u16, // FS-encoded date (yyyy-1980 << 9 | mm << 5 | dd)
    pub time: u16, // FS-encoded time (hh << 11 | mm << 5 | ss/2)
    pub tenth: u8, // optional tenths of second
}

#[derive(Debug, Clone)]
pub struct File {
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    pub attrs: FileAttrs,
    pub created: Option<FileTime>,
    pub accessed: Option<FileTime>,
    pub modified: Option<FileTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i32,
    pub month: u8, // 1..=12
    pub day: u8,   // 1..=31
    pub hour: u8,  // 0..=23
    pub min: u8,   // 0..=59
    pub sec: u8,   // 0..=59 (FAT stores 2-second ticks)
    pub tenth: u8, // 0..=199
}

impl FileTime {
    #[inline]
    pub fn to_datetime(self) -> Option<DateTime> {
        // Zero date often means "unknown"
        if self.date == 0 {
            return None;
        }

        let year = 1980 + ((self.date >> 9) as i32);
        let month = ((self.date >> 5) & 0x0F) as u8;
        let day = (self.date & 0x1F) as u8;

        let hour = ((self.time >> 11) & 0x1F) as u8;
        let min = ((self.time >> 5) & 0x3F) as u8;
        let sec = ((self.time & 0x1F) as u8) * 2;

        Some(DateTime {
            year,
            month,
            day,
            hour,
            min,
            sec,
            tenth: self.tenth,
        })
    }
}

// Mailbox between FS API callers and FS main thread
pub(super) static FS_REQUESTS: Once<Mailbox<FsRequest, FsResponse>> = Once::new();

#[derive(Debug, Clone)]
pub(super) enum FsRequest {
    // Global
    ListPartitions,
    ListMounts,
    Mount {
        index: usize,
        mount_point: Path,
    },
    Unmount {
        mount_point: Path,
    },
    // Path-based routing (global namespace)
    PathRequest {
        path: Path,
        op: PathOp,
    },
    // Partition routing
    PartitionRequest {
        index: usize,
        command: PartitionCommand,
    },
    // Internal for worker bootstrap
    GetPartitionMailbox(ThreadId),
}

#[derive(Debug, Clone)]
pub(super) enum FsResponse {
    Partitions(Vec<Partition>),
    MountPoints(alloc::collections::btree_map::BTreeMap<Path, usize>),
    Files(Result<Vec<File>, Error>),
    ReadBytes(Result<Vec<u8>, Error>),
    Written(Result<u64, Error>),
    File(Result<File, Error>),
    Ok(Result<(), Error>),
    // Internal
    PartitionMailbox(Option<Mailbox<PartitionCommand, FsResponse>>),
}

#[derive(Debug, Clone)]
pub(super) enum PathOp {
    ListFiles,
    ReadBytes { offset: usize, count: usize },
    WriteBytes { offset: usize, data: Vec<u8> },
    CreateFile,
    CreateDir,
    RemoveFile,
    RemoveDir,
    FileInfo,
    Flush,
}

#[derive(Debug, Clone)]
pub(super) enum PartitionCommand {
    ListFiles {
        path: Path,
    },
    ReadBytes {
        path: Path,
        offset: usize,
        count: usize,
    },
    WriteBytes {
        path: Path,
        offset: usize,
        data: Vec<u8>,
    },
    CreateFile {
        path: Path,
    },
    CreateDir {
        path: Path,
    },
    RemoveFile {
        path: Path,
    },
    RemoveDir {
        path: Path,
    },
    FileInfo {
        path: Path,
    },
    Flush,
}

pub extern "C" fn fs_main_thread() -> ! {
    let logger = sched().get_logger();
    let devices = list_devices();

    let requests = FS_REQUESTS.call_once(|| Mailbox::new(sched().current_id()));

    let mut partitions: Vec<Partition> = Vec::new();

    for device in &devices {
        match parse_gpt(device.id) {
            Ok(found_partitions) => {
                print_partitions(&found_partitions, &logger);
                partitions.extend(found_partitions);
            }
            Err(err) => log!(logger, "Error parsing GPT: {err}"),
        }
    }

    // Per-partition worker threads and their mailboxes
    let mut worker_mailboxes: Vec<Mailbox<PartitionCommand, FsResponse>> = Vec::new();
    let mut worker_tid_map = alloc::collections::btree_map::BTreeMap::<ThreadId, usize>::new();

    for (idx, partition) in partitions.iter().enumerate() {
        if let Some(filesystem) = &partition.filesystem {
            match filesystem {
                FilesystemType::Fat32 => {
                    let part = Box::new(partition.clone());
                    let part = &raw mut *Box::leak(part);
                    let worker_tid = queue_spawn_kthread_named_arg(
                        &format!("fs-partition-{}", idx),
                        fat32_partition_thread as u64,
                        part.cast(),
                    );
                    worker_tid_map.insert(worker_tid.clone(), idx);
                    worker_mailboxes.push(Mailbox::new(worker_tid));
                }
                FilesystemType::Unknown => {
                    // No worker for unknown FS
                }
            }
        }
    }

    // Mount table: map mount point to partition index
    let mut mount_points = alloc::collections::btree_map::BTreeMap::new();

    // Main loop: route and respond
    loop {
        while let Some(req) = requests.pop_request() {
            match req.message {
                FsRequest::ListPartitions => {
                    req.response
                        .send(FsResponse::Partitions(partitions.clone()));
                }
                FsRequest::ListMounts => {
                    req.response
                        .send(FsResponse::MountPoints(mount_points.clone()));
                }
                FsRequest::Mount { index, mount_point } => {
                    // Basic validation: index exists and mount point not used
                    let res =
                        if index < partitions.len() && !mount_points.contains_key(&mount_point) {
                            mount_points.insert(mount_point, index);
                            Ok(())
                        } else {
                            Err(Error::IoError)
                        };
                    req.response.send(FsResponse::Ok(res));
                }
                FsRequest::Unmount { mount_point } => {
                    let res = if mount_points.remove(&mount_point).is_some() {
                        Ok(())
                    } else {
                        Err(Error::IoError)
                    };
                    req.response.send(FsResponse::Ok(res));
                }
                FsRequest::PathRequest { path, op } => {
                    // Resolve longest-prefix mount point
                    let mut best: Option<(&Path, usize)> = None;
                    for (mp, &idx) in mount_points.iter() {
                        if path.starts_with(mp) {
                            match best {
                                None => best = Some((mp, idx)),
                                Some((best_mp, _)) => {
                                    if mp.components().len() > best_mp.components().len() {
                                        best = Some((mp, idx));
                                    }
                                }
                            }
                        }
                    }

                    if let Some((mp, part_idx)) = best {
                        let rel = path.strip_prefix(mp).normalize();
                        let cmd = match op {
                            PathOp::ListFiles => PartitionCommand::ListFiles { path: rel },
                            PathOp::ReadBytes { offset, count } => PartitionCommand::ReadBytes {
                                path: rel,
                                offset,
                                count,
                            },
                            PathOp::WriteBytes { offset, data } => PartitionCommand::WriteBytes {
                                path: rel,
                                offset,
                                data,
                            },
                            PathOp::CreateFile => PartitionCommand::CreateFile { path: rel },
                            PathOp::CreateDir => PartitionCommand::CreateDir { path: rel },
                            PathOp::RemoveFile => PartitionCommand::RemoveFile { path: rel },
                            PathOp::RemoveDir => PartitionCommand::RemoveDir { path: rel },
                            PathOp::FileInfo => PartitionCommand::FileInfo { path: rel },
                            PathOp::Flush => PartitionCommand::Flush,
                        };

                        if let Some(mb) = worker_mailboxes.get(part_idx) {
                            mb.forward(cmd, req.response);
                        } else {
                            req.response.send(FsResponse::Ok(Err(Error::IoError)));
                        }
                    } else {
                        // No mount matches this path
                        req.response.send(FsResponse::Ok(Err(Error::IoError)));
                    }
                }
                FsRequest::PartitionRequest { index, command } => {
                    if let Some(mb) = worker_mailboxes.get(index) {
                        mb.forward(command, req.response);
                    } else {
                        req.response.send(FsResponse::Ok(Err(Error::IoError)));
                    }
                }
                FsRequest::GetPartitionMailbox(tid) => {
                    if let Some(index) = worker_tid_map.get(&tid) {
                        let mb = worker_mailboxes.get(*index).cloned();
                        req.response.send(FsResponse::PartitionMailbox(mb));
                    } else {
                        req.response.send(FsResponse::PartitionMailbox(None));
                    }
                }
            }
        }

        sched().thread_park();
    }
}

extern "C" fn fat32_partition_thread(partition: *mut Partition) -> ! {
    let logger = sched().get_logger();
    let partition = unsafe { Box::from_raw(partition) };

    log!(logger, "Partition: {}({})", partition.index, partition.name);

    let Ok(mut fs) = Fat32fs::new((*partition).clone()) else {
        log!(logger, "Failed to create fat32");
        kthread_exit(-1)
    };

    // Get our mailbox from the FS main
    let mailbox = {
        use crate::fs::api::send_request as send;
        use core::time::Duration;
        loop {
            let resp = send(
                FsRequest::GetPartitionMailbox(sched().current_id()),
                Duration::from_secs(5),
            );
            if let FsResponse::PartitionMailbox(Some(mb)) = resp {
                break mb;
            }
        }
    };

    // Serve partition commands
    loop {
        while let Some(mut req) = mailbox.pop_request() {
            match req.message {
                PartitionCommand::ListFiles { path } => {
                    let res = (&fs as &dyn FileSystem).list_files(&path);
                    req.response.send(FsResponse::Files(res));
                }
                PartitionCommand::ReadBytes {
                    path,
                    offset,
                    count,
                } => {
                    let res = (&fs as &dyn FileSystem).read_bytes(&path, offset, count);
                    req.response.send(FsResponse::ReadBytes(res));
                }
                PartitionCommand::WriteBytes { path, offset, data } => {
                    let res = (&mut fs as &mut dyn FileSystem).write_bytes(&path, offset, &data);
                    req.response.send(FsResponse::Written(res));
                }
                PartitionCommand::CreateFile { path } => {
                    let res = (&mut fs as &mut dyn FileSystem).create_file(&path);
                    req.response.send(FsResponse::Ok(res));
                }
                PartitionCommand::CreateDir { path } => {
                    let res = (&mut fs as &mut dyn FileSystem).create_dir(&path);
                    req.response.send(FsResponse::Ok(res));
                }
                PartitionCommand::RemoveFile { path } => {
                    let res = (&mut fs as &mut dyn FileSystem).remove_file(&path);
                    req.response.send(FsResponse::Ok(res));
                }
                PartitionCommand::RemoveDir { path } => {
                    let res = (&mut fs as &mut dyn FileSystem).remove_dir(&path);
                    req.response.send(FsResponse::Ok(res));
                }
                PartitionCommand::FileInfo { path } => {
                    let res = (&fs as &dyn FileSystem).file_info(&path);
                    req.response.send(FsResponse::File(res));
                }
                PartitionCommand::Flush => {
                    let res = (&mut fs as &mut dyn FileSystem).flush();
                    req.response.send(FsResponse::Ok(res));
                }
            }
        }

        sched().thread_park();
    }
}
