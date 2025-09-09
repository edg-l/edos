use bytemuck::cast_ref;

use crate::{
    drivers::ahci::{api::read_sectors, structures::DeviceIdentifyInfo},
    fs::{
        Error, FileSystem,
        fat32::structures::{Fat32BootSector, FsInfo},
        gpt::Partition,
    },
    println,
};

pub mod read;
pub mod structures;
pub mod traverse;

#[derive(Debug)]
pub struct Fat32fs {
    pub boot_info: Fat32BootSector,
    pub fs_info: FsInfo,
    pub partition: Partition,
}

impl Fat32fs {
    pub fn new(partition: Partition) -> Result<Self, Error> {
        let boot_bytes = read_sectors(partition.device_id, partition.starting_lba, 1)?;

        if boot_bytes.len() != 512 {
            return Err(Error::MissingCriticalSectors);
        }

        let boot_info: &Fat32BootSector =
            cast_ref::<[u8; 512], _>(boot_bytes.as_slice().try_into().unwrap());

        if !boot_info.is_fat32() {
            return Err(Error::InvalidFs);
        }

        let fs_info_bytes = read_sectors(
            partition.device_id,
            partition.starting_lba + boot_info.fs_info as u64,
            1,
        )?;

        let fs_info: &FsInfo =
            cast_ref::<[u8; 512], _>(fs_info_bytes.as_slice().try_into().unwrap());

        if !fs_info.is_valid() {
            println!("Missing FsInfo, currently required");
            return Err(Error::InvalidFs);
        }

        Ok(Fat32fs {
            boot_info: *boot_info,
            fs_info: *fs_info,
            partition,
        })
    }
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
