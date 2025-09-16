#![expect(unused)]

use core::ffi::CStr;

use alloc::{boxed::Box, collections::btree_map::BTreeMap, format, string::String, vec::Vec};
use spin::Once;
use thiserror::Error;

use crate::{
    allocator::print_alloc_stats,
    drivers::ahci::{AhciError, api::list_devices},
    fs::{
        fat32::Fatfs,
        gpt::{FilesystemType, Partition, parse_gpt, print_partitions},
        mbr::parse_mbr,
        path::Path,
    },
    log,
    thread::{
        mailbox::Mailbox,
        scheduler::sched,
        util::{kthread_exit, queue_spawn_kthread_named, queue_spawn_kthread_named_arg},
    },
};

pub mod api;
pub mod block_device;
pub mod fat32;
pub mod gpt;
pub mod mbr;
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
    fn list_files(&mut self, path: &Path) -> Result<Vec<File>, Error>;
    fn read_bytes(&mut self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error>;
    fn write_bytes(&mut self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error>;
    fn create_file(&mut self, path: &Path) -> Result<(), Error>;
    fn create_dir(&mut self, path: &Path) -> Result<(), Error>;
    fn remove_dir(&mut self, path: &Path) -> Result<(), Error>;
    fn remove_file(&mut self, path: &Path) -> Result<(), Error>;
    fn file_info(&mut self, path: &Path) -> Result<File, Error>;
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

impl DateTime {
    /// Create DateTime from current RTC time
    pub fn now() -> Self {
        let rtc = crate::drivers::rtc::read_rtc();
        Self {
            year: rtc.year as i32,
            month: rtc.month,
            day: rtc.day,
            hour: rtc.hour,
            min: rtc.minute,
            sec: rtc.second,
            tenth: 0,
        }
    }

    /// Convert DateTime to FAT32 FileTime format
    pub fn to_file_time(self) -> FileTime {
        // FAT date: year-1980 (7 bits) | month (4 bits) | day (5 bits)
        let fat_date = (((self.year - 1980) as u16 & 0x7F) << 9)
            | ((self.month as u16 & 0x0F) << 5)
            | (self.day as u16 & 0x1F);

        // FAT time: hour (5 bits) | minute (6 bits) | second/2 (5 bits)
        let fat_time = ((self.hour as u16 & 0x1F) << 11)
            | ((self.min as u16 & 0x3F) << 5)
            | ((self.sec as u16 / 2) & 0x1F);

        FileTime {
            date: fat_date,
            time: fat_time,
            tenth: self.tenth,
        }
    }
}

impl FileTime {
    #[inline]
    pub fn to_datetime(self) -> Option<DateTime> {
        // Zero date means "unknown"
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

    pub fn now() -> Self {
        DateTime::now().to_file_time()
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
        device_id: usize,
        partition_index: usize,
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
        device_id: usize,
        partition_index: usize,
        command: PartitionCommand,
    },
    // Internal for worker bootstrap
    GetPartitionMailbox(u64), // threadid
}

#[derive(Debug, Clone)]
pub(super) enum FsResponse {
    Partitions(Vec<Partition>),
    MountPoints(alloc::collections::btree_map::BTreeMap<Path, (usize, usize)>),
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
    AddVirtualInfo {
        paths: Vec<Path>,
    },
}

// Helper functions for mount point handling

/// Find child mount points for a given path
fn find_child_mount_points(
    path: &Path,
    mount_points: &BTreeMap<Path, (usize, usize)>,
) -> Vec<String> {
    let parent_components = path.components();
    let mut child_mount_points = Vec::new();

    for child_mp in mount_points.keys() {
        let child_components = child_mp.components();
        // Check if this mount point is a direct child of the current path
        if child_components.len() == parent_components.len() + 1 {
            // Check if all parent components match
            let mut is_child = true;
            for (i, parent_comp) in parent_components.iter().enumerate() {
                if child_components[i] != *parent_comp {
                    is_child = false;
                    break;
                }
            }
            if is_child {
                let dir_name = child_components[parent_components.len()].clone();
                child_mount_points.push(dir_name);
            }
        }
    }

    child_mount_points
}

/// Convert PathOp to PartitionCommand
fn pathop_to_partition_command(op: PathOp, path: Path, real_path: Path) -> PartitionCommand {
    match op {
        PathOp::ListFiles => PartitionCommand::ListFiles { path },
        PathOp::ReadBytes { offset, count } => PartitionCommand::ReadBytes {
            path,
            offset,
            count,
        },
        PathOp::WriteBytes { offset, data } => PartitionCommand::WriteBytes { path, offset, data },
        PathOp::CreateFile => PartitionCommand::CreateFile { path },
        PathOp::CreateDir => PartitionCommand::CreateDir { path },
        PathOp::RemoveFile => PartitionCommand::RemoveFile { path },
        PathOp::RemoveDir => PartitionCommand::RemoveDir { path },
        PathOp::FileInfo => PartitionCommand::FileInfo { path: real_path },
        PathOp::Flush => PartitionCommand::Flush,
    }
}

/// Create a virtual directory file entry
fn create_virtual_file(name: String) -> File {
    File {
        name,
        kind: crate::fs::FileKind::Directory,
        size: 0,
        attrs: crate::fs::FileAttrs {
            readonly: false,
            hidden: false,
            system: false,
            archive: false,
        },
        created: None,
        accessed: None,
        modified: None,
    }
}

/// Find the best mount point for a given path (longest prefix match)
fn find_mount_at_path<'a>(
    path: &Path,
    mount_points: &'a BTreeMap<Path, (usize, usize)>,
) -> Option<(&'a Path, (usize, usize))> {
    let mut best: Option<(&'a Path, (usize, usize))> = None;
    for (mp, &idx) in mount_points.iter() {
        if mp.is_root() && best.is_none() {
            best = Some((mp, idx));
        } else if path.starts_with(mp) {
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
    best
}

pub extern "C" fn fs_main_thread() -> ! {
    let logger = sched().get_logger();
    let devices = list_devices();

    let requests = FS_REQUESTS.call_once(|| Mailbox::new(sched().current_id()));

    let mut partitions: Vec<Partition> = Vec::new();

    for device in &devices {
        match parse_gpt(device.id) {
            Ok(found_partitions) => {
                log!(logger, "GPT found on device {}", device.id);
                print_partitions(&found_partitions, &logger);
                partitions.extend(found_partitions);
            }
            Err(gpt_err) => {
                log!(logger, "GPT parsing failed: {}, trying MBR", gpt_err);
                match parse_mbr(device.id) {
                    Ok(found_partitions) => {
                        log!(logger, "MBR found on device {}", device.id);
                        crate::fs::mbr::print_partitions(&found_partitions, &logger);
                        partitions.extend(found_partitions);
                    }
                    Err(mbr_err) => {
                        log!(
                            logger,
                            "Both GPT and MBR parsing failed - GPT: {}, MBR: {}",
                            gpt_err,
                            mbr_err
                        );
                    }
                }
            }
        }
    }

    // Per-partition worker threads and their mailboxes
    let mut worker_mailboxes: BTreeMap<(usize, usize), Mailbox<PartitionCommand, FsResponse>> =
        BTreeMap::new();
    let mut worker_tid_map = alloc::collections::btree_map::BTreeMap::<u64, (usize, usize)>::new();

    for partition in partitions.iter() {
        if let Some(filesystem) = &partition.filesystem {
            match filesystem {
                FilesystemType::Fat12 | FilesystemType::Fat16 | FilesystemType::Fat32 => {
                    let part = Box::new(partition.clone());
                    let part = &raw mut *Box::leak(part);
                    let worker_tid = queue_spawn_kthread_named_arg(
                        &format!("fs-dev{}p{}", partition.device_id, partition.index),
                        fat_partition_thread as u64,
                        part.cast(),
                    );
                    worker_tid_map
                        .insert(worker_tid, (partition.device_id as usize, partition.index));
                    worker_mailboxes.insert(
                        (partition.device_id as usize, partition.index),
                        Mailbox::new(worker_tid),
                    );
                }
                FilesystemType::Ntfs | FilesystemType::Iso9660 | FilesystemType::Unknown => {
                    // No worker for these filesystem types yet
                    // TODO: Implement workers for NTFS and ISO9660
                }
            }
        }
    }

    // Mount table: map mount point to partition index
    let mut mount_points = alloc::collections::btree_map::BTreeMap::new();
    let mut mount_points_rev = alloc::collections::btree_map::BTreeMap::new();

    for part in &partitions {
        let idx = part.index;

        let path = Path::parse(&format!("/dev/sd{}p{}", part.device_id, part.index))
            .expect("failed to parse path");
        log!("Mounted {path}");
        mount_points.insert(path.clone(), (part.device_id as usize, part.index));
        mount_points_rev.insert((part.device_id as usize, part.index), path);
    }

    for (mb_id, mb) in &worker_mailboxes {
        let mut paths = Vec::new();
        if let Some(base_path) = mount_points_rev.get(mb_id) {
            for (path, id) in &mount_points {
                if *id != *mb_id
                    && let Some(parent) = base_path.parent()
                {
                    let stripped = path.strip_prefix(&parent);

                    if !stripped.is_root() {
                        paths.push(path.clone());
                    }
                }
            }

            if !paths.is_empty() {
                log!("Sending virtual info to {mb_id:?}: {paths:?}");
                mb.send(PartitionCommand::AddVirtualInfo { paths });
            }
        }
    }

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
                FsRequest::Mount {
                    device_id,
                    partition_index,
                    mount_point,
                } => {
                    log!(
                        "Mount request: {:?} at {:?}",
                        (device_id, partition_index),
                        mount_point
                    );
                    // Basic validation: index exists and mount point not used
                    let res = if !mount_points.contains_key(&mount_point) {
                        mount_points.insert(mount_point.clone(), (device_id, partition_index));
                        mount_points_rev.insert((device_id, partition_index), mount_point.clone());
                        Ok(())
                    } else {
                        Err(Error::IoError)
                    };
                    req.response.send(FsResponse::Ok(res));

                    let id = (device_id, partition_index);
                    for (mb_id, mb) in &worker_mailboxes {
                        let mut paths = Vec::new();
                        if let Some(base_path) = mount_points_rev.get(mb_id) {
                            for (path, id) in &mount_points {
                                if *id != *mb_id
                                    && path == &mount_point
                                    && let Some(parent) = base_path.parent()
                                {
                                    let stripped = path.strip_prefix(&parent);

                                    if !stripped.is_root() {
                                        paths.push(path.clone());
                                    }
                                }
                            }
                            if !paths.is_empty() {
                                log!("Sending virtual info to {mb_id:?}: {paths:?}");
                                mb.send(PartitionCommand::AddVirtualInfo { paths });
                            }
                        }
                    }
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
                    let mut mount_check_path = path.clone();

                    // If the op requests direct info of the given path, like file info, check the mount point at parent
                    // This makes it so cd /dev/sda1p1 works.
                    if matches!(op, PathOp::FileInfo)
                        && let Some(p) = path.parent()
                    {
                        mount_check_path = p;
                    }

                    // find mount point device/partition to route
                    if let Some((mount_path, part_idx)) =
                        find_mount_at_path(&mount_check_path, &mount_points)
                    {
                        // relative  path to the mount point.
                        let mut rel = path.strip_prefix(mount_path).normalize();

                        let cmd = pathop_to_partition_command(
                            op,
                            rel.clone(),
                            if &path == mount_path { path } else { rel },
                        );

                        if let Some(mb) = worker_mailboxes.get(&part_idx) {
                            mb.forward(cmd, req.response);
                        } else {
                            req.response.send(FsResponse::Ok(Err(Error::FileNotFound)));
                        }
                    } else {
                        req.response.send(FsResponse::Ok(Err(Error::FileNotFound)));
                    }
                }
                FsRequest::PartitionRequest {
                    partition_index,
                    device_id,
                    command,
                } => {
                    if let Some(mb) = worker_mailboxes.get(&(device_id, partition_index)) {
                        mb.forward(command, req.response);
                    } else {
                        req.response.send(FsResponse::Ok(Err(Error::IoError)));
                    }
                }
                FsRequest::GetPartitionMailbox(tid) => {
                    if let Some(index) = worker_tid_map.get(&tid) {
                        let mb = worker_mailboxes.get(index).cloned();
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

extern "C" fn fat_partition_thread(partition: *mut Partition) -> ! {
    let logger = sched().get_logger();
    let partition = unsafe { Box::from_raw(partition) };

    log!(
        logger,
        "Partition: /dev/sd{}p{} ({})",
        partition.device_id,
        partition.index,
        partition.name
    );

    let Ok(mut fs) = Fatfs::new((*partition).clone()) else {
        log!(logger, "Failed to create FAT filesystem");
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

    let mut virtual_files: BTreeMap<Path, File> = BTreeMap::new();

    // Serve partition commands
    loop {
        while let Some(mut req) = mailbox.pop_request() {
            match req.message {
                PartitionCommand::ListFiles { path } => {
                    if let Some(file) = virtual_files.get(&path) {
                        req.response
                            .send(FsResponse::Files(Ok(alloc::vec![file.clone()])));
                    } else {
                        let mut res = fs.list_files(&path);

                        if let Ok(res) = &mut res {
                            for f in &virtual_files {
                                if path.is_direct_parent(f.0) {
                                    res.push(f.1.clone());
                                }
                            }
                        }
                        req.response.send(FsResponse::Files(res));
                    }
                }
                PartitionCommand::ReadBytes {
                    path,
                    offset,
                    count,
                } => {
                    let res = fs.read_bytes(&path, offset, count);
                    req.response.send(FsResponse::ReadBytes(res));
                }
                PartitionCommand::WriteBytes { path, offset, data } => {
                    let res = fs.write_bytes(&path, offset, &data);
                    req.response.send(FsResponse::Written(res));
                }
                PartitionCommand::CreateFile { path } => {
                    let res = fs.create_file(&path);
                    req.response.send(FsResponse::Ok(res));
                }
                PartitionCommand::CreateDir { path } => {
                    let res = fs.create_dir(&path);
                    req.response.send(FsResponse::Ok(res));
                }
                PartitionCommand::RemoveFile { path } => {
                    let res = fs.remove_file(&path);
                    req.response.send(FsResponse::Ok(res));
                }
                PartitionCommand::RemoveDir { path } => {
                    let res = fs.remove_dir(&path);
                    req.response.send(FsResponse::Ok(res));
                }
                PartitionCommand::FileInfo { path } => {
                    if let Some(file) = virtual_files.get(&path) {
                        req.response.send(FsResponse::File(Ok(file.clone())));
                    } else {
                        let res = fs.file_info(&path);
                        req.response.send(FsResponse::File(res));
                    }
                }
                PartitionCommand::Flush => {
                    let res = fs.flush();
                    req.response.send(FsResponse::Ok(res));
                }
                PartitionCommand::AddVirtualInfo { paths } => {
                    for path in paths {
                        let file = create_virtual_file(path.components().last().unwrap().clone());
                        virtual_files.insert(path, file);
                    }
                }
            }
        }

        sched().thread_park();
    }
}
