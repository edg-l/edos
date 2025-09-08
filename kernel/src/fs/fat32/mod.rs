use crate::{
    drivers::ahci::structures::DeviceIdentifyInfo,
    fs::{
        FileSystem,
        fat32::structures::{Fat32BootSector, FsInfo},
    },
};

pub mod structures;

#[derive(Debug)]
pub struct Fat32fs {
    pub boot_info: Fat32BootSector,
    pub fs_info: FsInfo,
    pub device_id: u64,
    pub device_info: DeviceIdentifyInfo,
}

impl Fat32fs {
    pub fn new(device_id: u64) {}
}

impl FileSystem for Fat32fs {
    fn list_files(
        &self,
        path: super::path::Path,
    ) -> Result<alloc::vec::Vec<super::FileInfo>, super::Error> {
        todo!()
    }

    fn read_bytes(
        &self,
        path: super::path::Path,
        offset: usize,
        count: usize,
    ) -> Result<alloc::vec::Vec<u8>, super::Error> {
        todo!()
    }

    fn write_bytes(
        &self,
        path: super::path::Path,
        offset: usize,
        data: alloc::vec::Vec<u8>,
    ) -> Result<u64, super::Error> {
        todo!()
    }

    fn create_file(&self, path: super::path::Path) -> Result<(), super::Error> {
        todo!()
    }

    fn create_dir(&self, path: super::path::Path) -> Result<(), super::Error> {
        todo!()
    }

    fn remove_dir(&self, path: super::path::Path) -> Result<(), super::Error> {
        todo!()
    }

    fn remove_file(&self, path: super::path::Path) -> Result<(), super::Error> {
        todo!()
    }

    fn file_info(&self, path: super::path::Path) -> Result<super::FileInfo, super::Error> {
        todo!()
    }
}
