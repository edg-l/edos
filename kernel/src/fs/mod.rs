use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use spin::{Mutex, Once};
use thiserror::Error;

use crate::{
    drivers::ahci::{AhciError, api::list_devices},
    fs::{
        efs::EfsDriver,
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
        mailbox::Mailbox, runqueue::IO_PRIORITY, scheduler::sched, util::queue_spawn_kthread_named,
    },
};

pub mod api;
pub mod block_device;
pub mod dentry;
pub mod devfs;
pub mod efs;
pub mod fat32;
pub mod gpt;
pub mod handle;
pub mod inode;
pub mod mbr;
pub mod memfs;
pub mod path;
pub mod procfs;
pub mod vfs;

pub use devfs::{DevFsDevice, DevFsError, DevFsHandle as DevFs, register_device_str};

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

    #[expect(unused)]
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

    #[expect(unused)]
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
    #[expect(unused)]
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
    // Read-only operations (&self) -- can run concurrently via RwLock.
    fn list_files(&self, path: &Path) -> Result<Vec<File>, Error>;
    fn read_bytes(&self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error>;
    fn file_info(&self, path: &Path) -> Result<File, Error>;

    fn poll(&self, _path: &Path) -> Result<Box<dyn Pollable>, Error> {
        Err(Error::IoError)
    }

    fn mmap(
        &self,
        _path: &Path,
        _offset: usize,
        _length: usize,
        _memory: Arc<Mutex<MemoryManager>>,
    ) -> Result<MmapRegion, Error> {
        Err(Error::IoError)
    }

    fn statfs(&self) -> Result<StatFs, Error> {
        Err(Error::Unsupported)
    }

    /// Return a filesystem-local inode number for the given path.
    /// Used by the VFS dentry cache to identify inodes.
    /// Default returns 0 (suitable for stateless filesystems like procfs).
    fn resolve_inode(&self, path: &Path) -> Result<u64, Error> {
        // Stateless filesystems don't have meaningful inode numbers.
        let _ = path;
        Ok(0)
    }

    // Write/mutating operations (&self) -- each driver manages its own locking.
    fn write_bytes(&self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error>;
    fn create_file(&self, path: &Path) -> Result<(), Error>;
    fn create_dir(&self, path: &Path) -> Result<(), Error>;
    fn remove_dir(&self, path: &Path) -> Result<(), Error>;
    fn remove_file(&self, path: &Path) -> Result<(), Error>;
    fn flush(&self) -> Result<(), Error>;

    fn ioctl(&self, _path: &Path, _request: u64, _arg: u64) -> Result<u64, Error> {
        Err(Error::IoError)
    }

    fn truncate(&self, _path: &Path, _size: u64) -> Result<(), Error> {
        Err(Error::IoError)
    }

    fn rename(&self, _old_path: &Path, _new_path: &Path) -> Result<(), Error> {
        Err(Error::IoError)
    }
}

/// Filesystem statistics returned by the `statfs` trait method.
#[derive(Debug, Clone)]
pub struct StatFs {
    /// Filesystem type name (e.g. "efs", "fat32").
    pub fs_type: &'static str,
    /// Block size in bytes.
    pub block_size: u64,
    /// Total blocks.
    pub total_blocks: u64,
    /// Free blocks.
    pub free_blocks: u64,
    /// Total inodes (0 if not applicable).
    pub total_inodes: u64,
    /// Free inodes (0 if not applicable).
    pub free_inodes: u64,
    /// Volume label.
    pub volume_name: [u8; 64],
    /// Filesystem format version.
    pub version: u32,
    /// Number of block groups (0 if not applicable).
    pub block_groups: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    #[expect(unused)]
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
    Mount {
        device_id: usize,
        partition_index: usize,
        mount_point: Path,
        fstype: FilesystemType,
    },
    Unmount {
        mount_point: Path,
    },
    // Dynamically register a partition (e.g. USB storage discovered after boot)
    RegisterPartition {
        partition: Partition,
    },
}

#[derive(Debug)]
pub(super) enum FsResponse {
    Partitions(Vec<Partition>),
    Ok(Result<(), Error>),
}

pub extern "C" fn fs_main_thread() -> ! {
    log!("Started main fs");
    let thread = sched().current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);
    let devices = list_devices();
    log!("Listed devices");

    let requests = FS_REQUESTS.call_once(|| Arc::new(Mailbox::with_capacity(16)));

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

    // Main loop: handle management requests
    loop {
        let mut req = requests.recv();
        let payload = req.payload.take().unwrap();
        match payload {
            FsRequest::ListPartitions => {
                req.reply(FsResponse::Partitions(partitions.clone()));
            }
            FsRequest::RegisterPartition { partition } => {
                log!(
                    "fs: registered partition: {} (device {})",
                    partition.name,
                    partition.device_id
                );
                partitions.push(partition);
                req.reply(FsResponse::Ok(Ok(())));
            }
            FsRequest::Mount {
                device_id,
                partition_index,
                mount_point,
                fstype,
            } => {
                log!(
                    "Mount request: {:?} ({fstype:?}) at {:?}",
                    (device_id, partition_index),
                    mount_point
                );

                if vfs::is_mount_point(&mount_point) {
                    req.reply(FsResponse::Ok(Err(Error::IoError)));
                    continue;
                }

                match fstype {
                    FilesystemType::Fat12 | FilesystemType::Fat16 | FilesystemType::Fat32 => {
                        let mut mounted = false;
                        for partition in partitions.iter() {
                            if partition.device_id as usize == device_id
                                && partition.index == partition_index
                                && partition.filesystem.is_some()
                            {
                                match Fatfs::new(partition.clone()) {
                                    Ok(fat_fs) => {
                                        vfs::mount(
                                            mount_point.clone(),
                                            vfs::MountEntry {
                                                fs: Arc::new(fat_fs),
                                                mount_id: 0,
                                                device_id,
                                                partition_index,
                                                filesystem: fstype.clone(),
                                            },
                                        );
                                        mounted = true;
                                    }
                                    Err(e) => {
                                        log!("Failed to create FAT filesystem: {:?}", e);
                                    }
                                }
                                break;
                            }
                        }
                        if !mounted {
                            req.reply(FsResponse::Ok(Err(Error::IoError)));
                            continue;
                        }
                    }
                    FilesystemType::Memfs => {
                        log!("Mounting memfs");
                        match Memfs::new() {
                            Ok(memfs) => {
                                vfs::mount(
                                    mount_point.clone(),
                                    vfs::MountEntry {
                                        fs: Arc::new(memfs),
                                        mount_id: 0,
                                        device_id: 0,
                                        partition_index: 0,
                                        filesystem: FilesystemType::Memfs,
                                    },
                                );
                            }
                            Err(e) => {
                                log!("Failed to create memfs: {:?}", e);
                                req.reply(FsResponse::Ok(Err(Error::IoError)));
                                continue;
                            }
                        }
                    }
                    FilesystemType::Devfs => {
                        log!("Mounting devfs");
                        match DevFs::new() {
                            Ok(devfs) => {
                                vfs::mount(
                                    mount_point.clone(),
                                    vfs::MountEntry {
                                        fs: Arc::new(devfs),
                                        mount_id: 0,
                                        device_id: 0,
                                        partition_index: 0,
                                        filesystem: FilesystemType::Devfs,
                                    },
                                );
                            }
                            Err(e) => {
                                log!("Failed to create devfs: {:?}", e);
                                req.reply(FsResponse::Ok(Err(Error::IoError)));
                                continue;
                            }
                        }
                    }
                    FilesystemType::Procfs => {
                        log!("Mounting procfs");
                        match Procfs::new() {
                            Ok(procfs) => {
                                vfs::mount(
                                    mount_point.clone(),
                                    vfs::MountEntry {
                                        fs: Arc::new(procfs),
                                        mount_id: 0,
                                        device_id: 0,
                                        partition_index: 0,
                                        filesystem: FilesystemType::Procfs,
                                    },
                                );
                            }
                            Err(e) => {
                                log!("Failed to create procfs: {:?}", e);
                                req.reply(FsResponse::Ok(Err(Error::IoError)));
                                continue;
                            }
                        }
                    }
                    FilesystemType::Efs => {
                        let mut mounted = false;
                        for partition in partitions.iter() {
                            if partition.device_id as usize == device_id
                                && partition.index == partition_index
                                && partition.filesystem.is_some()
                            {
                                match EfsDriver::new(partition.clone()) {
                                    Ok(efs_fs) => {
                                        vfs::mount(
                                            mount_point.clone(),
                                            vfs::MountEntry {
                                                fs: Arc::new(efs_fs),
                                                mount_id: 0,
                                                device_id,
                                                partition_index,
                                                filesystem: fstype.clone(),
                                            },
                                        );
                                        mounted = true;
                                    }
                                    Err(e) => {
                                        log!("Failed to mount EFS: {:?}", e);
                                    }
                                }
                                break;
                            }
                        }
                        if !mounted {
                            req.reply(FsResponse::Ok(Err(Error::IoError)));
                            continue;
                        }
                    }
                    FilesystemType::Unknown | FilesystemType::Iso9660 | FilesystemType::Ntfs => {
                        req.reply(FsResponse::Ok(Err(Error::IoError)));
                        continue;
                    }
                }

                req.reply(FsResponse::Ok(Ok(())));
            }
            FsRequest::Unmount { mount_point } => {
                let res = if vfs::unmount(&mount_point) {
                    Ok(())
                } else {
                    Err(Error::IoError)
                };
                req.reply(FsResponse::Ok(res));
            }
        }
    }
}
