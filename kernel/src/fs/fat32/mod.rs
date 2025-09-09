use alloc::vec::Vec;
use bytemuck::cast_ref;

use crate::{
    drivers::ahci::{
        api::{read_sectors, write_sectors},
        structures::DeviceIdentifyInfo,
    },
    fs::{
        Error, File, FileSystem,
        fat32::structures::{DirectoryEntry, Fat32BootSector, FsInfo},
        gpt::Partition,
    },
    println,
};

pub mod read;
pub mod structures;
pub mod traverse;
pub mod write;

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
    ) -> Result<alloc::vec::Vec<super::File>, super::Error> {
        let path = path.normalize();

        let entries;
        if path.is_root() {
            entries = self.get_dir_entries(self.boot_info.root_cluster)?;
        } else if let Some((entry, _, _)) = self.find_dir_entry(&path)? {
            if !entry.is_directory() {
                return Err(Error::NotADir);
            }
            entries = self.get_dir_entries(entry.first_cluster())?;
        } else {
            return Err(Error::FileNotFound);
        }

        let mut files = Vec::new();
        for entry in entries {
            files.push(File::from(entry));
        }

        Ok(files)
    }

    fn read_bytes(
        &self,
        path: super::path::Path,
        offset: usize,
        count: usize,
    ) -> Result<alloc::vec::Vec<u8>, super::Error> {
        if let Some((entry, _, _)) = self.find_dir_entry(&path)? {
            self.read_file_offset(&entry, offset, count)
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn write_bytes(
        &mut self,
        path: super::path::Path,
        offset: usize,
        data: alloc::vec::Vec<u8>,
    ) -> Result<u64, super::Error> {
        // Locate file and write payload
        let (entry, entry_cluster, entry_off) = match self.find_dir_entry(&path)? {
            Some((e, c, o)) if !e.is_directory() => (e, c, o),
            Some(_) => return Err(Error::NotAFile),
            None => return Err(Error::FileNotFound),
        };

        let written = self.write_file_offset(&entry, offset as u64, &data)? as u64;
        let new_size = core::cmp::max(entry.file_size as u64, offset as u64 + written);

        // Patch size and archive bit in-place
        self.patch_dir_entry_at(entry_cluster, entry_off, |de| {
            de.file_size = new_size as u32;
            de.attributes |= 0x20; // ARCHIVE
            // TODO: set write_time/write_date here if you have a clock
        })?;

        Ok(written)
    }

    fn create_file(&mut self, path: super::path::Path) -> Result<(), super::Error> {
        todo!()
    }

    fn create_dir(&mut self, path: super::path::Path) -> Result<(), super::Error> {
        todo!()
    }

    fn remove_dir(&mut self, path: super::path::Path) -> Result<(), super::Error> {
        todo!()
    }

    fn remove_file(&mut self, path: super::path::Path) -> Result<(), super::Error> {
        todo!()
    }

    fn file_info(&self, path: super::path::Path) -> Result<super::File, super::Error> {
        if let Some((entry, _, _)) = self.find_dir_entry(&path)? {
            Ok(entry.into())
        } else {
            Err(Error::FileNotFound)
        }
    }
}
