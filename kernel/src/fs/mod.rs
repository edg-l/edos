use crate::thread::preempt::PreemptSpinlock;
use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use spin::Once;
use thiserror::Error;

use crate::thread::scheduler::current_thread;
use crate::{
    drivers::{
        ahci::{AhciError, api::list_devices},
        block_io,
    },
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
    thread::{mailbox::Mailbox, runqueue::IO_PRIORITY, util::queue_spawn_kthread_named},
};

pub mod api;
pub mod block_device;
pub mod block_page_cache;
pub mod dentry;
pub mod devfs;
pub mod efs;
pub mod evict;
pub mod fat32;
pub mod gpt;
pub mod handle;
pub mod icache;
pub mod inode;
pub mod journal;
pub mod mbr;
pub mod memfs;
pub mod page_cache;
pub mod page_fill;
pub mod path;
pub mod procfs;
pub mod readahead;
pub mod vfs;
pub mod writeback;

#[allow(unused_imports)]
pub use page_fill::PageFillHandle;

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
    #[error("device or resource busy")]
    Busy,
    #[error("invalid argument")]
    InvalidArgument,
    #[error("too many levels of symbolic links")]
    TooManyLinks,
    #[error("filesystem thread answered a request with the wrong reply")]
    ProtocolMismatch,
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

/// Maximum symbolic links a single path resolution may traverse.
pub const MAX_SYMLINK_HOPS: u32 = 8;

/// Splice a symbolic link's target into the path being resolved. `prefix` is
/// the components resolved before the link, `rest` the ones that followed it.
/// An absolute target discards the prefix; `.` and `..` are folded lexically.
pub fn splice_symlink(prefix: &[String], target: &str, rest: &[String]) -> Vec<String> {
    let mut out: Vec<String> = if target.starts_with('/') {
        Vec::new()
    } else {
        prefix.to_vec()
    };
    for component in target.split('/').chain(rest.iter().map(String::as_str)) {
        match component {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other.to_string()),
        }
    }
    out
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
        _memory: Arc<PreemptSpinlock<MemoryManager>>,
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

    /// Create a symbolic link at `path` holding `target` verbatim.
    fn symlink(&self, _target: &str, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    /// Read the target of the symbolic link at `path` without following it.
    fn read_link(&self, _path: &Path) -> Result<String, Error> {
        Err(Error::Unsupported)
    }

    /// Stamp the access and modification times, in whole Unix seconds. `None`
    /// leaves that timestamp as it stands.
    fn set_times(
        &self,
        _path: &Path,
        _atime: Option<u64>,
        _mtime: Option<u64>,
    ) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    // --- Inode-based fast-path methods ---
    // Drivers that support direct inode access override these to skip path walks.
    // Default returns Unsupported; the VFS falls back to path-based methods.

    fn read_bytes_ino(&self, _ino: u64, _offset: usize, _count: usize) -> Result<Vec<u8>, Error> {
        Err(Error::Unsupported)
    }

    fn write_bytes_ino(&self, _ino: u64, _offset: usize, _data: &[u8]) -> Result<u64, Error> {
        Err(Error::Unsupported)
    }

    fn file_size_ino(&self, _ino: u64) -> Result<u64, Error> {
        Err(Error::Unsupported)
    }

    fn flush_inode(&self, _ino: u64) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    /// Return a reference to `PageCacheOps` if this filesystem supports it.
    /// Default returns None (stateless filesystems like procfs/memfs/devfs).
    fn as_page_cache_ops(&self) -> Option<&dyn crate::fs::page_cache::PageCacheOps> {
        None
    }

    /// Free on-disk resources for an inode that was unlinked while still
    /// referenced. Called by `VfsInode::drop` when `orphan == true` on the
    /// final Arc release, modelling Linux's `evict_inode`. The VFS guarantees
    /// no live fds or VMAs exist at this point. Default: no-op (stateless FS).
    fn evict_inode(&self, _ino: u64) -> Result<(), Error> {
        Ok(())
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

    /// Seconds since 1970-01-01T00:00:00Z.
    pub fn to_unix_secs(self) -> u64 {
        let days = days_since_epoch(self.year as i64, self.month as i64, self.day as i64);
        if days < 0 {
            return 0;
        }
        days as u64 * 86400 + self.hour as u64 * 3600 + self.min as u64 * 60 + self.sec as u64
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

    /// Encode a Unix timestamp. The FS encoding keeps seconds in 2-second
    /// ticks and cannot represent anything before 1980, so those inputs land
    /// on the epoch's first representable instant.
    pub fn from_unix_secs(secs: u64) -> Self {
        let sec = (secs % 60) as u8;
        let total_mins = secs / 60;
        let min = (total_mins % 60) as u8;
        let total_hours = total_mins / 60;
        let hour = (total_hours % 24) as u8;
        let mut days = (total_hours / 24) as u32;

        let mut year = 1970i32;
        loop {
            let days_in_year: u32 = if is_leap_year(year) { 366 } else { 365 };
            if days < days_in_year {
                break;
            }
            days -= days_in_year;
            year += 1;
        }

        let months: [u32; 12] = if is_leap_year(year) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };
        let mut month = 1u8;
        for &m in &months {
            if days < m {
                break;
            }
            days -= m;
            month += 1;
        }
        let day = days as u8 + 1;

        DateTime {
            year,
            month,
            day,
            hour,
            min,
            sec,
            tenth: 0,
        }
        .to_file_time()
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days from 1970-01-01 to the given civil date (Howard Hinnant's `days_from_civil`).
fn days_since_epoch(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 12 } else { month };
    365 * y + y / 4 - y / 100 + y / 400 + (153 * (m - 3) + 2) / 5 + day - 719469
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
    // Re-read one device's partition table, e.g. after `edos-install` wrote a
    // new one through /dev/sda.
    RescanPartitions {
        device_id: u64,
    },
}

#[derive(Debug)]
pub(super) enum FsResponse {
    Partitions(Vec<Partition>),
    Ok(Result<(), Error>),
    Count(Result<usize, Error>),
}

/// Read one block device's partition table. GPT first, MBR as a fallback, an
/// empty list when neither parses.
///
/// Boot-time discovery and `RescanPartitions` both go through here, so a disk
/// partitioned by `edos-install` is described exactly like one found at boot.
fn scan_device(device_id: u64) -> Vec<Partition> {
    // Skip CD/DVD (ATAPI) devices. We have no ISO9660 filesystem driver and
    // the first ATAPI READ on a freshly-initialized device is ~600ms under
    // QEMU's emulated media-ready latency. When CD-ROM support lands, mount it
    // explicitly via the mount syscall — detect_filesystem still handles ATAPI
    // correctly.
    if crate::drivers::ahci::is_atapi(device_id) {
        log!("Skipping ATAPI device {}", device_id);
        return Vec::new();
    }

    match parse_gpt(device_id) {
        Ok(found) => {
            log!("GPT found on device {}", device_id);
            print_partitions(&found);
            found
        }
        Err(gpt_err) => {
            log!("GPT parsing failed on device {device_id}: {gpt_err}, trying MBR");
            match parse_mbr(device_id) {
                Ok(found) => {
                    log!("MBR found on device {}", device_id);
                    crate::fs::mbr::print_partitions(&found);
                    found
                }
                Err(mbr_err) => {
                    log!(
                        "Both GPT and MBR parsing failed on device {device_id} - GPT: {gpt_err}, MBR: {mbr_err}"
                    );
                    Vec::new()
                }
            }
        }
    }
}

pub extern "C" fn fs_main_thread() -> ! {
    log!("Started main fs");
    let thread = current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);

    // AHCI publishes its ports into the block-io registry at the end of its
    // probe, and `list_devices` blocks until that happens. Waiting here keeps
    // boot-time discovery deterministic; the ids themselves come from the
    // registry, which also holds the live-root ramdisk and, later, USB storage.
    let _ = list_devices();
    log!("Listed devices");

    let requests = FS_REQUESTS.call_once(|| Arc::new(Mailbox::with_capacity(16)));

    let mut partitions: Vec<Partition> = Vec::new();

    for device_id in block_io::list() {
        partitions.extend(scan_device(device_id));
    }

    // Raw device nodes, so a disk with no partition table is still reachable
    // from userspace (that is what `edos-install` starts from).
    devfs::block::register_all();

    // Main loop: handle management requests
    loop {
        let mut req = requests.recv();
        let payload = req.payload.take().unwrap();
        match payload {
            FsRequest::ListPartitions => {
                req.reply(FsResponse::Partitions(partitions.clone()));
            }
            FsRequest::RegisterPartition { partition } => {
                // Idempotent: a device registered in block_io just before the
                // boot scan ran can be described by both paths.
                let known = partitions
                    .iter()
                    .any(|p| p.device_id == partition.device_id && p.index == partition.index);
                if known {
                    log!(
                        "fs: partition {} (device {}) already known",
                        partition.name,
                        partition.device_id
                    );
                } else {
                    log!(
                        "fs: registered partition: {} (device {})",
                        partition.name,
                        partition.device_id
                    );
                    partitions.push(partition);
                }
                // The device behind it appeared after boot, so it has no node yet.
                devfs::block::register_all();
                req.reply(FsResponse::Ok(Ok(())));
            }
            FsRequest::RescanPartitions { device_id } => {
                let result = if vfs::list_mounts()
                    .iter()
                    .any(|m| m.device_id as u64 == device_id && m.filesystem.is_device_backed())
                {
                    log!("fs: refusing to rescan device {device_id}, it backs a mount");
                    Err(Error::Busy)
                } else {
                    // Drop cached blocks first: the caller wrote this device
                    // through /dev/sd*, and anything still cached from the
                    // previous table would be read back by the next mount.
                    block_page_cache::BlockPageCache::global().invalidate_device(device_id);
                    partitions.retain(|p| p.device_id != device_id);
                    let found = scan_device(device_id);
                    let count = found.len();
                    partitions.extend(found);
                    log!("fs: rescanned device {device_id}, {count} partition(s)");
                    Ok(count)
                };
                req.reply(FsResponse::Count(result));
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
