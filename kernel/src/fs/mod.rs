#![expect(unused)]

use alloc::{string::String, vec::Vec};
use thiserror::Error;

use crate::{fs::path::Path, thread::util::queue_spawn_kthread_named};

pub mod api;
pub mod fat32;
pub mod gpt;
pub mod path;

pub fn init() {
    queue_spawn_kthread_named("fs", api::fs_main_thread as u64);
}

#[derive(Debug, Error, Clone)]
pub enum Error {
    #[error("file not found")]
    FileNotFound,
    #[error("i/o error")]
    IoError,
}

pub trait FileSystem {
    fn list_files(&self, path: Path) -> Result<Vec<FileInfo>, Error>;

    fn read_bytes(&self, path: Path, offset: usize, count: usize) -> Result<Vec<u8>, Error>;

    fn write_bytes(&self, path: Path, offset: usize, data: Vec<u8>) -> Result<u64, Error>;

    fn create_file(&self, path: Path) -> Result<(), Error>;
    fn create_dir(&self, path: Path) -> Result<(), Error>;
    fn remove_dir(&self, path: Path) -> Result<(), Error>;
    fn remove_file(&self, path: Path) -> Result<(), Error>;

    fn file_info(&self, path: Path) -> Result<FileInfo, Error>;
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub writeable: bool,
    pub is_dir: bool,
    pub creation_time: u64,
    pub last_access_date: u64,
    pub file_size: u64,
}
