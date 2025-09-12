#![expect(unused)]

use core::ffi::CStr;

use alloc::{boxed::Box, format, string::String, vec::Vec};
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
        scheduler::sched,
        util::{kthread_exit, queue_spawn_kthread_named, queue_spawn_kthread_named_arg},
    },
};

pub mod api;
pub mod block_device;
pub mod fat32;
pub mod gpt;
pub mod path;
pub mod vfs;

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

pub extern "C" fn fs_main_thread() -> ! {
    let logger = sched().get_logger();
    let devices = list_devices();

    let mut partitions = Vec::new();

    for device in &devices {
        match parse_gpt(device.id) {
            Ok(found_partitions) => {
                print_partitions(&found_partitions, &logger);
                partitions.extend(found_partitions);
            }
            Err(err) => log!(logger, "Error parsing GPT: {err}"),
        }
    }

    // Maybe for each partition create a thread, and use this thread to route requests?

    for partition in &partitions {
        if let Some(filesystem) = &partition.filesystem {
            match filesystem {
                FilesystemType::Fat32 => {
                    let part = Box::new(partition.clone());
                    let part = &raw mut *Box::leak(part);
                    queue_spawn_kthread_named_arg(
                        &format!("fat32-fs-{}", partition.index),
                        fat32_partition_thread as u64,
                        part.cast(),
                    );
                }
                FilesystemType::Unknown => {}
            }
        }
    }

    loop {
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

    let bytes = fs.boot_info.bytes_per_sector;
    log!(logger, "FAT32 bytes per sector: {}", bytes);
    log!(
        logger,
        "FAT32 sectors per cluster: {}",
        fs.boot_info.sectors_per_cluster
    );

    // Some test code, remove when implementing properly

    let entries = fs.get_dir_entries(fs.boot_info.root_cluster).unwrap();

    log!(logger, "Showing root /");
    for entry in &entries {
        log!(logger, "Name: {}", entry.fat_name_to_string());
        log!(logger, "Is dir: {}", entry.is_directory());

        if entry.is_directory() {
            let entries = fs.get_dir_entries(entry.first_cluster()).unwrap();

            for entry in &entries {
                log!(logger, "Name: {}", entry.fat_name_to_string());
                log!(logger, "Is dir: {}", entry.is_directory());
            }
        } else {
            let content = fs.read_file(entry).unwrap();
            let x = core::str::from_utf8(&content);
            if let Ok(x) = x {
                log!(logger, "Content:\n{x:?}");
            }
        }
    }

    log!(logger, "Using the api");

    let fs = (&mut fs) as &mut dyn FileSystem;

    let files = fs.list_files(&Path::parse_str("/").unwrap()).unwrap();

    for file in files {
        log!(logger, "Name: {}", file.name);
        log!(
            logger,
            "Created: {:?}",
            file.created.map(|x| x.to_datetime())
        );
    }

    let path = Path::parse_str("/edgar.txt").unwrap();
    fs.create_file(&path).unwrap();
    print_alloc_stats();

    log!(logger, "created file");

    fs.write_bytes(&path, 0, c"hello written".to_bytes_with_nul())
        .unwrap();

    log!(logger, "wrote bytes");

    let content = fs.read_bytes(&path, 0, 512).unwrap();

    let content = CStr::from_bytes_with_nul(&content);

    log!(logger, "Content: {content:?}");

    print_alloc_stats();

    //

    loop {
        sched().thread_park();
    }
}
