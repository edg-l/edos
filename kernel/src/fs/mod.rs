#![expect(unused)]

use alloc::{string::String, vec::Vec};
use thiserror::Error;

use crate::{drivers::ahci::AhciError, fs::path::Path, thread::util::queue_spawn_kthread_named};

pub mod api;
pub mod block_device;
pub mod fat32;
pub mod gpt;
pub mod path;
pub mod vfs;

pub fn init() {
    queue_spawn_kthread_named("fs", api::fs_main_thread as u64);
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
