#![expect(unused)]

use core::{
    ffi::{CStr, c_void},
    time::Duration,
};

use alloc::{
    boxed::Box,
    collections::btree_map::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use spin::{Mutex, Once, RwLock};
use thiserror::Error;

use crate::{
    allocator::print_alloc_stats,
    drivers::ahci::{AhciError, api::list_devices},
    fs::{
        fat32::Fatfs,
        gpt::{FilesystemType, Partition, parse_gpt, print_partitions},
        handle::Pollable,
        mbr::parse_mbr,
        memfs::Memfs,
        path::Path,
        procfs::Procfs,
    },
    log,
    memory::mapper::MemoryManager,
    thread::{
        mailbox::Mailbox,
        runqueue::{DEFAULT_PRIORITY, IO_PRIORITY},
        scheduler::sched,
        thread::ThreadId,
        util::{kthread_exit, queue_spawn_kthread_named, queue_spawn_kthread_named_arg},
    },
};

pub mod api;
pub mod block_device;
pub mod devfs;
pub mod fat32;
pub mod gpt;
pub mod handle;
pub mod mbr;
pub mod memfs;
pub mod path;
pub mod procfs;

pub use devfs::{
    DevFsDevice, DevFsError, DevFsHandle as DevFs, register_device, register_device_str,
    unregister_device, unregister_device_str,
};

pub fn init() {
    queue_spawn_kthread_named("fs", fs_main_thread as *const () as u64);
}

#[expect(clippy::enum_variant_names)]
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
    #[error("unsupported op")]
    Unsupported,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollState {
    pub readable: bool,
    pub writable: bool,
    pub error: bool,
    pub hangup: bool,
    pub invalid: bool,
}

impl PollState {
    pub const fn none() -> Self {
        Self {
            readable: false,
            writable: false,
            error: false,
            hangup: false,
            invalid: false,
        }
    }

    pub const fn with_readable() -> Self {
        Self {
            readable: true,
            writable: false,
            error: false,
            hangup: false,
            invalid: false,
        }
    }

    pub fn matches(&self, interests: Self) -> bool {
        let mut matched = false;

        if interests.readable && self.readable {
            matched = true;
        }
        if interests.writable && self.writable {
            matched = true;
        }
        if interests.error && self.error {
            matched = true;
        }
        if interests.hangup && self.hangup {
            matched = true;
        }
        if interests.invalid && self.invalid {
            matched = true;
        }

        if !interests.readable
            && !interests.writable
            && !interests.error
            && !interests.hangup
            && !interests.invalid
        {
            matched = self.readable || self.writable || self.error || self.hangup || self.invalid;
        }

        matched
    }

    pub fn merge(&mut self, other: Self) {
        self.readable |= other.readable;
        self.writable |= other.writable;
        self.error |= other.error;
        self.hangup |= other.hangup;
        self.invalid |= other.invalid;
    }

    pub const fn to_bits(self) -> u8 {
        (self.readable as u8)
            | ((self.writable as u8) << 1)
            | ((self.error as u8) << 2)
            | ((self.hangup as u8) << 3)
            | ((self.invalid as u8) << 4)
    }

    pub const fn from_bits(bits: u8) -> Self {
        Self {
            readable: (bits & 0x01) != 0,
            writable: (bits & 0x02) != 0,
            error: (bits & 0x04) != 0,
            hangup: (bits & 0x08) != 0,
            invalid: (bits & 0x10) != 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmapRegion {
    pub phys_addr: u64,
    pub length: usize,
    pub writable: bool,
    pub cacheable: bool,
}

impl MmapRegion {
    pub const fn new(phys_addr: u64, length: usize) -> Self {
        Self {
            phys_addr,
            length,
            writable: false,
            cacheable: false,
        }
    }
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

    fn ioctl(&mut self, _path: &Path, _request: u64, _arg: u64) -> Result<u64, Error> {
        Err(Error::IoError)
    }

    fn poll(&mut self, _path: &Path) -> Result<Box<dyn Pollable>, Error> {
        Err(Error::IoError)
    }

    fn mmap(
        &mut self,
        _path: &Path,
        _offset: usize,
        _length: usize,
        memory: Arc<Mutex<MemoryManager>>,
    ) -> Result<MmapRegion, Error> {
        Err(Error::IoError)
    }
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

#[derive(Debug, Clone)]
pub struct MountInfo {
    pub mount_point: Path,
    pub device_id: usize,
    pub partition_index: usize,
    pub filesystem: FilesystemType,
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
pub(super) static FS_REQUESTS: Once<Arc<Mailbox<FsRequest, FsResponse>>> = Once::new();

#[derive(Debug, Clone)]
pub(super) enum FsRequest {
    // Global
    ListPartitions,
    ListMounts,
    Mount {
        device_id: usize,
        partition_index: usize,
        mount_point: Path,
        fstype: FilesystemType,
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
    FsThreadRequest {
        device_id: usize,
        partition_index: usize,
        command: FsThreadCommand,
    },
    // Internal for worker bootstrap
    GetPartitionMailbox(ThreadId), // threadid
}

#[derive(Debug)]
pub(super) enum FsResponse {
    Partitions(Vec<Partition>),
    Mounts(Vec<MountInfo>),
    Files(Result<Vec<File>, Error>),
    ReadBytes(Result<Vec<u8>, Error>),
    Written(Result<u64, Error>),
    File(Result<File, Error>),
    Ok(Result<(), Error>),
    Ioctl(Result<u64, Error>),
    Poll(Result<Box<dyn Pollable>, Error>),
    Mmap(Result<MmapRegion, Error>),
    // Internal
    PartitionMailbox(Option<Arc<Mailbox<FsThreadCommand, FsResponse>>>),
}

#[derive(Debug, Clone)]
pub(super) enum PathOp {
    ListFiles,
    ReadBytes {
        offset: usize,
        count: usize,
    },
    WriteBytes {
        offset: usize,
        data: Vec<u8>,
    },
    CreateFile,
    CreateDir,
    RemoveFile,
    RemoveDir,
    FileInfo,
    Flush,
    Ioctl {
        request: u64,
        arg: u64,
    },
    Poll,
    Mmap {
        offset: usize,
        length: usize,
        memory: Arc<Mutex<MemoryManager>>,
    },
}

// TODO: Add rmdir recursive in a atomic command

#[derive(Debug, Clone)]
pub(super) enum FsThreadCommand {
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
    Ioctl {
        path: Path,
        request: u64,
        arg: u64,
    },
    Poll {
        path: Path,
    },
    Mmap {
        path: Path,
        offset: usize,
        length: usize,
        memory: Arc<Mutex<MemoryManager>>,
    },
    AddVirtualInfo {
        paths: Vec<Path>,
    },
}

// Helper functions for mount point handling

/// Find child mount points for a given path
#[derive(Debug, Clone)]
struct MountMetadata {
    device_id: usize,
    partition_index: usize,
    filesystem: FilesystemType,
}

fn find_child_mount_points(
    path: &Path,
    mount_points: &BTreeMap<Path, MountMetadata>,
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
fn pathop_to_partition_command(op: PathOp, path: Path, real_path: Path) -> FsThreadCommand {
    match op {
        PathOp::ListFiles => FsThreadCommand::ListFiles { path },
        PathOp::ReadBytes { offset, count } => FsThreadCommand::ReadBytes {
            path,
            offset,
            count,
        },
        PathOp::WriteBytes { offset, data } => FsThreadCommand::WriteBytes { path, offset, data },
        PathOp::CreateFile => FsThreadCommand::CreateFile { path },
        PathOp::CreateDir => FsThreadCommand::CreateDir { path },
        PathOp::RemoveFile => FsThreadCommand::RemoveFile { path },
        PathOp::RemoveDir => FsThreadCommand::RemoveDir { path },
        PathOp::FileInfo => FsThreadCommand::FileInfo { path: real_path },
        PathOp::Flush => FsThreadCommand::Flush,
        PathOp::Ioctl { request, arg } => FsThreadCommand::Ioctl { path, request, arg },
        PathOp::Poll => FsThreadCommand::Poll { path },
        PathOp::Mmap {
            offset,
            length,
            memory,
        } => FsThreadCommand::Mmap {
            path,
            offset,
            length,
            memory,
        },
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
    mount_points: &'a BTreeMap<Path, MountMetadata>,
) -> Option<(&'a Path, &'a MountMetadata)> {
    let mut best: Option<(&'a Path, &'a MountMetadata)> = None;
    for (mp, meta) in mount_points.iter() {
        if mp.is_root() && best.is_none() {
            best = Some((mp, meta));
        } else if path.starts_with(mp) {
            match best {
                None => best = Some((mp, meta)),
                Some((best_mp, _)) => {
                    if mp.components().len() > best_mp.components().len() {
                        best = Some((mp, meta));
                    }
                }
            }
        }
    }
    best
}

pub static FS_WORKER_MAILBOXES: RwLock<
    BTreeMap<(usize, usize), Arc<Mailbox<FsThreadCommand, FsResponse>>>,
> = RwLock::new(BTreeMap::new());

pub extern "C" fn fs_main_thread() -> ! {
    log!("Started main fs");
    let thread = sched().current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);
    let devices = list_devices();
    log!("Listed devices");

    let requests = FS_REQUESTS.call_once(|| Arc::new(Mailbox::new()));

    let mut partitions: Vec<Partition> = Vec::new();

    for device in &devices {
        match parse_gpt(device.id) {
            Ok(found_partitions) => {
                log!("GPT found on device {}", device.id);
                print_partitions(&found_partitions);
                partitions.extend(found_partitions);
            }
            Err(gpt_err) => {
                log!("GPT parsing failed: {}, trying MBR", gpt_err);
                match parse_mbr(device.id) {
                    Ok(found_partitions) => {
                        log!("MBR found on device {}", device.id);
                        crate::fs::mbr::print_partitions(&found_partitions);
                        partitions.extend(found_partitions);
                    }
                    Err(mbr_err) => {
                        log!(
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
    let mut worker_tid_map = BTreeMap::<ThreadId, (usize, usize)>::new();

    // Mount table: map mount point to partition index
    let mut mount_points: BTreeMap<Path, MountMetadata> = BTreeMap::new();
    let mut mount_points_rev: BTreeMap<(usize, usize), Path> = BTreeMap::new();

    for (mb_id, mb) in FS_WORKER_MAILBOXES.read().iter() {
        let mut paths = Vec::new();
        if let Some(base_path) = mount_points_rev.get(mb_id) {
            for (path, meta) in &mount_points {
                if (meta.device_id, meta.partition_index) != *mb_id
                    && let Some(parent) = base_path.parent()
                {
                    let stripped = path.strip_prefix(&parent);

                    if !stripped.is_root() {
                        paths.push(path.clone());
                    }
                }
            }

            if !paths.is_empty() {
                mb.send(FsThreadCommand::AddVirtualInfo { paths });
            }
        }
    }

    const SPECIAL_DEV_ID: usize = 9000;
    let mut current_special_part = 0;

    // Main loop: route and respond
    loop {
        let mut req = requests.recv();
        let payload = req.payload.take().unwrap();
        {
            match payload {
                FsRequest::ListPartitions => {
                    req.reply(FsResponse::Partitions(partitions.clone()));
                }
                FsRequest::ListMounts => {
                    let mounts = mount_points
                        .iter()
                        .map(|(path, meta)| MountInfo {
                            mount_point: path.clone(),
                            device_id: meta.device_id,
                            partition_index: meta.partition_index,
                            filesystem: meta.filesystem.clone(),
                        })
                        .collect();
                    req.reply(FsResponse::Mounts(mounts));
                }
                FsRequest::Mount {
                    mut device_id,
                    mut partition_index,
                    mut mount_point,
                    mut fstype,
                } => {
                    log!(
                        "Mount request: {:?} ({fstype:?}) at {:?}",
                        (device_id, partition_index),
                        mount_point
                    );

                    if mount_points.contains_key(&mount_point) {
                        req.reply(FsResponse::Ok(Err(Error::IoError)));
                        continue;
                    }

                    match fstype {
                        FilesystemType::Fat12 | FilesystemType::Fat16 | FilesystemType::Fat32 => {
                            for partition in partitions.iter() {
                                if partition.device_id as usize == device_id
                                    && partition.index == partition_index
                                    && let Some(filesystem) = &partition.filesystem
                                {
                                    let part = Box::new(partition.clone());
                                    let part = &raw mut *Box::leak(part);

                                    let name = match filesystem {
                                        FilesystemType::Memfs => {
                                            format!("fs-memfs-{}", partition.index)
                                        }
                                        FilesystemType::Fat32 => {
                                            format!(
                                                "fs-fat32-{}p{}",
                                                partition.device_id, partition.index
                                            )
                                        }
                                        _ => {
                                            format!(
                                                "fs-dev{}p{}",
                                                partition.device_id, partition.index
                                            )
                                        }
                                    };

                                    let worker_tid = queue_spawn_kthread_named_arg(
                                        &name,
                                        start_partition_fs_thread as *const () as u64,
                                        part.cast(),
                                    );
                                    worker_tid_map.insert(
                                        worker_tid,
                                        (partition.device_id as usize, partition.index),
                                    );
                                    FS_WORKER_MAILBOXES.write().insert(
                                        (partition.device_id as usize, partition.index),
                                        Mailbox::new().into(),
                                    );
                                    break;
                                }
                            }
                        }
                        FilesystemType::Memfs => {
                            log!("Mounting memfs");
                            let fs = Box::into_raw(Box::new(
                                Memfs::new().expect("failed to create memfs"),
                            ));
                            let worker_tid = queue_spawn_kthread_named_arg(
                                &format!("fs-{}", mount_point.filename()),
                                start_memfs_thread as *const () as u64,
                                fs.cast(),
                            );
                            device_id = SPECIAL_DEV_ID;
                            partition_index = current_special_part;
                            worker_tid_map
                                .insert(worker_tid, (SPECIAL_DEV_ID, current_special_part));
                            FS_WORKER_MAILBOXES.write().insert(
                                (SPECIAL_DEV_ID, current_special_part),
                                Mailbox::new().into(),
                            );
                            current_special_part += 1;
                        }
                        FilesystemType::Devfs => {
                            log!("Mounting devfs");
                            let fs = Box::into_raw(Box::new(
                                DevFs::new().expect("failed to create devfs"),
                            ));
                            let worker_tid = queue_spawn_kthread_named_arg(
                                &format!("devfs-{}", mount_point.filename()),
                                start_devfs_thread as *const () as u64,
                                fs.cast(),
                            );
                            device_id = SPECIAL_DEV_ID;
                            partition_index = current_special_part;
                            worker_tid_map
                                .insert(worker_tid, (SPECIAL_DEV_ID, current_special_part));
                            FS_WORKER_MAILBOXES.write().insert(
                                (SPECIAL_DEV_ID, current_special_part),
                                Mailbox::new().into(),
                            );
                            current_special_part += 1;
                        }
                        FilesystemType::Procfs => {
                            log!("Mounting procfs");
                            let fs = Box::into_raw(Box::new(
                                Procfs::new().expect("failed to create procfs"),
                            ));
                            let worker_tid = queue_spawn_kthread_named_arg(
                                &format!("procfs-{}", mount_point.filename()),
                                start_procfs_thread as *const () as u64,
                                fs.cast(),
                            );
                            device_id = SPECIAL_DEV_ID;
                            partition_index = current_special_part;
                            worker_tid_map
                                .insert(worker_tid, (SPECIAL_DEV_ID, current_special_part));
                            FS_WORKER_MAILBOXES.write().insert(
                                (SPECIAL_DEV_ID, current_special_part),
                                Mailbox::new().into(),
                            );
                            current_special_part += 1;
                        }
                        FilesystemType::Unknown
                        | FilesystemType::Iso9660
                        | FilesystemType::Ntfs => {
                            req.reply(FsResponse::Ok(Err(Error::IoError)));
                            continue;
                        }
                    }

                    let meta = MountMetadata {
                        device_id,
                        partition_index,
                        filesystem: fstype.clone(),
                    };

                    mount_points.insert(mount_point.clone(), meta.clone());
                    mount_points_rev.insert((device_id, partition_index), mount_point.clone());

                    if let Some(parent_path) = mount_point.parent()
                        && let Some((_parent_mount, parent_meta)) =
                            find_mount_at_path(&parent_path, &mount_points)
                    {
                        let parent_key = (parent_meta.device_id, parent_meta.partition_index);

                        if let Some(mb) = FS_WORKER_MAILBOXES.read().get(&parent_key) {
                            let paths = alloc::vec![mount_point.clone()];
                            log!("Sending virtual info for {} to {parent_key:?}", mount_point);
                            mb.send(FsThreadCommand::AddVirtualInfo { paths });
                        }
                    }

                    req.reply(FsResponse::Ok(Ok(())));
                }
                FsRequest::Unmount { mount_point } => {
                    let res = if let Some(meta) = mount_points.remove(&mount_point) {
                        mount_points_rev.remove(&(meta.device_id, meta.partition_index));
                        Ok(())
                    } else {
                        Err(Error::IoError)
                    };
                    req.reply(FsResponse::Ok(res));
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
                    if let Some((mount_path, meta)) =
                        find_mount_at_path(&mount_check_path, &mount_points)
                    {
                        // relative  path to the mount point.
                        let mut rel = path.strip_prefix(mount_path).normalize();

                        let cmd = pathop_to_partition_command(
                            op.clone(),
                            rel.clone(),
                            if &path == mount_path {
                                path.clone()
                            } else {
                                rel
                            },
                        );

                        let mailbox_key = (meta.device_id, meta.partition_index);

                        if let Some(mb) = FS_WORKER_MAILBOXES.read().get(&mailbox_key) {
                            mb.forward(req, cmd);
                        } else {
                            req.reply(FsResponse::Ok(Err(Error::FileNotFound)));
                        }
                    } else {
                        req.reply(FsResponse::Ok(Err(Error::FileNotFound)));
                    }
                }
                FsRequest::FsThreadRequest {
                    partition_index,
                    device_id,
                    command,
                } => {
                    if let Some(mb) = FS_WORKER_MAILBOXES
                        .read()
                        .get(&(device_id, partition_index))
                    {
                        mb.forward(req, command.clone());
                    } else {
                        req.reply(FsResponse::Ok(Err(Error::IoError)));
                    }
                }
                FsRequest::GetPartitionMailbox(tid) => {
                    if let Some(index) = worker_tid_map.get(&tid) {
                        let mb = FS_WORKER_MAILBOXES.read().get(index).cloned();
                        req.reply(FsResponse::PartitionMailbox(mb));
                    } else {
                        req.reply(FsResponse::PartitionMailbox(None));
                    }
                }
            }
        }
    }
}

extern "C" fn start_partition_fs_thread(partition: *mut Partition) -> ! {
    let partition = unsafe { Box::from_raw(partition) };

    let path = if partition.filesystem == Some(FilesystemType::Memfs) {
        Path::parse(&format!("/dev/memfs{}", partition.index)).expect("failed to parse path")
    } else {
        Path::parse(&format!(
            "/dev/sd{}p{}",
            partition.device_id, partition.index
        ))
        .expect("failed to parse path")
    };

    log!("Partition: {} ({})", path, partition.name);

    // Get our mailbox from the FS main
    let mailbox = {
        use crate::fs::api::send_request as send;
        use core::time::Duration;
        loop {
            let resp = send(
                FsRequest::GetPartitionMailbox(sched().current_thread_id().unwrap()),
                Duration::from_secs(5),
            );
            if let FsResponse::PartitionMailbox(Some(mb)) = resp {
                break mb;
            }
        }
    };

    let mut fs: Box<dyn FileSystem>;

    match &partition.filesystem {
        Some(ty) => match ty {
            FilesystemType::Fat12 | FilesystemType::Fat16 | FilesystemType::Fat32 => {
                let Ok(mut fat_fs) = Fatfs::new((*partition).clone()) else {
                    log!("Failed to create FAT filesystem");
                    kthread_exit(-1)
                };
                fs = Box::new(fat_fs);
            }
            FilesystemType::Memfs => {
                let Ok(mut memfs) = Memfs::new() else {
                    log!("Failed to create Memfs filesystem");
                    kthread_exit(-1)
                };
                fs = Box::new(memfs);
            }
            FilesystemType::Devfs => {
                log!("Devfs cannot be backed by a partition");
                unsupported_fs(mailbox);
            }
            FilesystemType::Procfs => {
                log!("Procfs cannot be backed by a partition");
                unsupported_fs(mailbox);
            }
            FilesystemType::Ntfs => {
                log!("NTFS not yet implemented");
                unsupported_fs(mailbox);
            }
            FilesystemType::Iso9660 => {
                log!("Iso9660 not yet implemented");
                unsupported_fs(mailbox);
            }
            FilesystemType::Unknown => {
                log!("Unknown fs type");
                unsupported_fs(mailbox);
            }
        },
        None => unsupported_fs(mailbox),
    }

    run_fs_thread(fs);
}

extern "C" fn start_memfs_thread(fs: *mut Memfs) -> ! {
    let fs: Box<dyn FileSystem> = unsafe { Box::from_raw(fs) };
    run_fs_thread(fs)
}

extern "C" fn start_devfs_thread(fs: *mut DevFs) -> ! {
    let fs: Box<dyn FileSystem> = unsafe { Box::from_raw(fs) };
    run_fs_thread(fs)
}

extern "C" fn start_procfs_thread(fs: *mut Procfs) -> ! {
    let fs: Box<dyn FileSystem> = unsafe { Box::from_raw(fs) };
    run_fs_thread(fs)
}

fn run_fs_thread(mut fs: Box<dyn FileSystem>) -> ! {
    log!("Fs thread started");
    let thread = sched().current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);
    let mut virtual_files: BTreeMap<Path, File> = BTreeMap::new();

    // Get our mailbox from the FS main
    let mailbox = {
        use crate::fs::api::send_request as send;
        use core::time::Duration;
        loop {
            let resp = send(
                FsRequest::GetPartitionMailbox(sched().current_thread_id().unwrap()),
                Duration::from_secs(5),
            );
            if let FsResponse::PartitionMailbox(Some(mb)) = resp {
                break mb;
            }
        }
    };

    // Serve partition commands
    loop {
        let mut req = mailbox.recv();
        let message = req.payload.take().unwrap();
        {
            match message {
                FsThreadCommand::ListFiles { path } => {
                    if let Some(file) = virtual_files.get(&path) {
                        req.reply(FsResponse::Files(Ok(alloc::vec![file.clone()])));
                    } else {
                        let mut res = fs.list_files(&path);

                        if let Ok(res) = &mut res {
                            for f in &virtual_files {
                                if path.is_direct_parent(f.0) {
                                    res.push(f.1.clone());
                                }
                            }
                        }
                        req.reply(FsResponse::Files(res));
                    }
                }
                FsThreadCommand::ReadBytes {
                    path,
                    offset,
                    count,
                } => {
                    let res = fs.read_bytes(&path, offset, count);
                    req.reply(FsResponse::ReadBytes(res));
                }
                FsThreadCommand::WriteBytes { path, offset, data } => {
                    let res = fs.write_bytes(&path, offset, &data);
                    req.reply(FsResponse::Written(res));
                }
                FsThreadCommand::CreateFile { path } => {
                    let res = fs.create_file(&path);
                    req.reply(FsResponse::Ok(res));
                }
                FsThreadCommand::CreateDir { path } => {
                    let res = fs.create_dir(&path);
                    req.reply(FsResponse::Ok(res));
                }
                FsThreadCommand::RemoveFile { path } => {
                    let res = fs.remove_file(&path);
                    req.reply(FsResponse::Ok(res));
                }
                FsThreadCommand::RemoveDir { path } => {
                    let res = fs.remove_dir(&path);
                    req.reply(FsResponse::Ok(res));
                }
                FsThreadCommand::FileInfo { path } => {
                    if let Some(file) = virtual_files.get(&path) {
                        req.reply(FsResponse::File(Ok(file.clone())));
                    } else {
                        let res = fs.file_info(&path);
                        req.reply(FsResponse::File(res));
                    }
                }
                FsThreadCommand::Flush => {
                    let res = fs.flush();
                    req.reply(FsResponse::Ok(res));
                }
                FsThreadCommand::Ioctl { path, request, arg } => {
                    let res = fs.ioctl(&path, request, arg);
                    req.reply(FsResponse::Ioctl(res));
                }
                FsThreadCommand::Poll { path } => {
                    let res = fs.poll(&path);
                    req.reply(FsResponse::Poll(res));
                }
                FsThreadCommand::Mmap {
                    path,
                    offset,
                    length,
                    memory,
                } => {
                    let res = fs.mmap(&path, offset, length, memory);
                    req.reply(FsResponse::Mmap(res));
                }
                FsThreadCommand::AddVirtualInfo { paths } => {
                    for path in paths {
                        let file = create_virtual_file(path.components().last().unwrap().clone());
                        virtual_files.insert(path.clone(), file);
                    }
                }
            }
        }
    }
}

fn unsupported_fs(mb: Arc<Mailbox<FsThreadCommand, FsResponse>>) -> ! {
    loop {
        let req = mb.recv();
        req.reply(FsResponse::Ok(Err(Error::IoError)));
    }
}
