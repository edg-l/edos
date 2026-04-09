use alloc::vec::Vec;
use bytemuck::{Zeroable, cast_ref};

use crate::{
    fs::{
        Error, File, FileSystem, FileTime,
        block_device::BlockDevice,
        fat32::structures::{ATTR_LONG_NAME, DirectoryEntry, Fat32BootSector, FatVariant, FsInfo},
        gpt::Partition,
    },
    log,
};

use super::path::Path;

pub mod read;
pub mod structures;
pub mod traverse;
pub mod write;

#[derive(Debug)]
pub struct Fatfs {
    pub boot_info: Fat32BootSector,
    pub fs_info: Option<FsInfo>,
    pub variant: FatVariant,
    pub partition: Partition,
    pub device: BlockDevice,
}

impl Fatfs {
    pub fn new(partition: Partition) -> Result<Self, Error> {
        let mut device = BlockDevice::new(partition.device_id, 4096);
        let boot_bytes = device.read_sectors(partition.starting_lba, 1, Vec::new())?;

        if boot_bytes.len() != 512 {
            return Err(Error::MissingCriticalSectors);
        }

        let boot_info: Fat32BootSector =
            *cast_ref::<[u8; 512], _>(boot_bytes.as_slice().try_into().unwrap());

        // Determine FAT variant based on cluster count
        let variant = boot_info.determine_fat_variant().ok_or(Error::InvalidFs)?;

        // For FAT32, validate strictly and read FSInfo
        let fs_info = match variant {
            FatVariant::Fat32 => {
                if !boot_info.is_fat32() {
                    return Err(Error::InvalidFs);
                }

                let fs_info_bytes = device.read_sectors(
                    partition.starting_lba + boot_info.fs_info as u64,
                    1,
                    boot_bytes,
                )?;

                let fs_info: FsInfo =
                    *cast_ref::<[u8; 512], _>(fs_info_bytes.as_slice().try_into().unwrap());

                if !fs_info.is_valid() {
                    log!("Missing FsInfo, currently required for FAT32");
                    return Err(Error::InvalidFs);
                }

                Some(fs_info)
            }
            FatVariant::Fat12 | FatVariant::Fat16 => {
                // FAT12/16 don't have FSInfo
                None
            }
        };

        Ok(Fatfs {
            boot_info,
            fs_info,
            variant,
            device,
            partition,
        })
    }
}

impl FileSystem for Fatfs {
    fn list_files(&mut self, path: &Path) -> Result<alloc::vec::Vec<super::File>, super::Error> {
        let path = path.normalize();

        let entries;
        if path.is_root() {
            entries = match self.variant {
                FatVariant::Fat32 => self.get_dir_entries(self.boot_info.root_cluster)?,
                FatVariant::Fat12 | FatVariant::Fat16 => self.get_root_dir_entries()?,
            };
        } else if let Some((entry, _, _)) = self.find_dir_entry(&path)? {
            if !entry.is_directory() {
                return Err(Error::NotADir);
            }
            entries = self.get_dir_entries(entry.first_cluster())?;
        } else {
            return Err(Error::FileNotFound);
        }

        let mut files = Vec::new();
        for entry in entries.iter() {
            files.push(File::from(entry));
        }

        Ok(files)
    }

    fn read_bytes(
        &mut self,
        path: &Path,
        offset: usize,
        count: usize,
    ) -> Result<alloc::vec::Vec<u8>, super::Error> {
        if let Some((entry, _, _)) = self.find_dir_entry(path)? {
            self.read_file_offset(&entry, offset, count)
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn write_bytes(
        &mut self,
        path: &Path,
        offset: usize,
        data: &[u8],
    ) -> Result<u64, super::Error> {
        if data.is_empty() {
            return Ok(0);
        }

        // Locate entry + its on-disk position
        let (mut entry, ec, eo) = match self.find_dir_entry(path)? {
            Some((e, c, o)) if !e.is_directory() => (e, c, o),
            Some(_) => return Err(Error::NotAFile),
            None => return Err(Error::FileNotFound),
        };

        // Write. Pass the on-disk position so a head cluster can be patched if needed.
        let written =
            self.write_file_offset(&mut entry, Some((ec, eo)), offset as u64, data)? as u64;

        // Update metadata: size (+ archive bit).
        let new_size = core::cmp::max(entry.file_size as u64, offset as u64 + written);
        let current_time = crate::fs::FileTime::now();

        self.patch_dir_entry_at(ec, eo, |de| {
            de.file_size = new_size as u32;
            de.attributes |= 0x20; // ARCHIVE
            de.write_date = current_time.date;
            de.write_time = current_time.time;
        })?;

        Ok(written)
    }

    fn create_file(&mut self, path: &Path) -> Result<(), super::Error> {
        use crate::fs::fat32::structures::DirectoryEntry;
        use bytemuck::Zeroable;

        // Resolve parent and leaf name
        let (parent_cluster, name) = self.resolve_parent_and_name(path)?;

        // Already exists?
        if (self.find_dir_entry(path)?).is_some() {
            return Err(Error::IoError);
        }

        let (short_name, needs_lfn) = self.generate_short_name(parent_cluster, &name)?;

        // Build short entry
        let mut de: DirectoryEntry = DirectoryEntry::zeroed();
        de.set_name_from_string(&short_name);
        de.attributes = 0x20; // ARCHIVE
        de.first_cluster_high = 0;
        de.first_cluster_low = 0;
        de.file_size = 0;

        let current_time = FileTime::now();
        de.creation_date = current_time.date;
        de.creation_time = current_time.time;
        de.creation_time_tenth = current_time.tenth;
        de.write_date = current_time.date;
        de.write_time = current_time.time;
        de.last_access_date = current_time.date;

        // Append to parent directory
        let long_name = needs_lfn.then_some(name.as_str());
        let _ = self.append_dir_entry(parent_cluster, &de, long_name)?;
        Ok(())
    }

    fn create_dir(&mut self, path: &Path) -> Result<(), super::Error> {
        // Resolve parent and leaf name
        let (parent_cluster, name) = self.resolve_parent_and_name(path)?;

        // Already exists?
        if (self.find_dir_entry(path)?).is_some() {
            return Err(Error::IoError);
        }

        // Allocate directory cluster
        let newc = self.alloc_cluster()?;

        let current_time = FileTime::now();

        // Initialize "." and ".."
        let spc = self.boot_info.sectors_per_cluster as u16;
        let bps = self.boot_info.bytes_per_sector as usize;
        let cluster_bytes = bps * spc as usize;
        let mut dirbuf = alloc::vec![0u8; cluster_bytes];

        let mut dot: DirectoryEntry = DirectoryEntry::zeroed();
        dot.set_name_from_string(".");
        dot.attributes = 0x10; // DIRECTORY
        dot.first_cluster_high = ((newc >> 16) & 0xFFFF) as u16;
        dot.first_cluster_low = (newc & 0xFFFF) as u16;
        dot.creation_date = current_time.date;
        dot.creation_time = current_time.time;
        dot.write_date = current_time.date;
        dot.write_time = current_time.time;
        dot.last_access_date = current_time.date;

        let mut dotdot: DirectoryEntry = DirectoryEntry::zeroed();
        dotdot.set_name_from_string("..");
        dotdot.attributes = 0x10; // DIRECTORY
        dotdot.first_cluster_high = ((parent_cluster >> 16) & 0xFFFF) as u16;
        dotdot.first_cluster_low = (parent_cluster & 0xFFFF) as u16;
        dotdot.creation_date = current_time.date;
        dotdot.creation_time = current_time.time;
        dotdot.write_date = current_time.date;
        dotdot.write_time = current_time.time;
        dotdot.last_access_date = current_time.date;

        let dot_bytes: [u8; 32] = bytemuck::cast(dot);
        let dotdot_bytes: [u8; 32] = bytemuck::cast(dotdot);
        dirbuf[0..32].copy_from_slice(&dot_bytes);
        dirbuf[32..64].copy_from_slice(&dotdot_bytes);

        self.device
            .write_sectors(self.cluster_to_lba(newc), dirbuf, spc)?;

        // Insert directory entry in parent
        let (short_name, needs_lfn) = self.generate_short_name(parent_cluster, &name)?;

        let mut de: DirectoryEntry = DirectoryEntry::zeroed();
        de.set_name_from_string(&short_name);
        de.attributes = 0x10; // DIRECTORY
        de.first_cluster_high = ((newc >> 16) & 0xFFFF) as u16;
        de.first_cluster_low = (newc & 0xFFFF) as u16;
        de.file_size = 0;
        de.creation_date = current_time.date;
        de.creation_time = current_time.time;
        de.write_date = current_time.date;
        de.write_time = current_time.time;
        de.last_access_date = current_time.date;

        let long_name = needs_lfn.then_some(name.as_str());
        let _ = self.append_dir_entry(parent_cluster, &de, long_name)?;
        Ok(())
    }

    fn remove_file(&mut self, path: &Path) -> Result<(), super::Error> {
        let (parent_cluster, _) = self.resolve_parent_and_name(path)?;

        // Locate the entry and its position in parent directory
        let (entry, dir_cluster, entry_off) = match self.find_dir_entry(path)? {
            Some((e, c, o)) => (e, c, o),
            None => return Err(Error::FileNotFound),
        };
        if entry.is_directory() {
            return Err(Error::NotAFile);
        }

        // Free the file's cluster chain, if any
        let start = entry.first_cluster();
        if start >= 2 {
            let _freed = self.free_cluster_chain(start)?;
        }

        // Mark the directory entry deleted (0xE5)
        let (base_lba, sectors) = self.dir_entry_region(dir_cluster);
        let mut buf = Vec::new();
        buf = self.device.read_sectors(base_lba, sectors, buf)?;
        if entry_off + 32 > buf.len() {
            return Err(Error::IoError);
        }
        buf[entry_off] = 0xE5;
        self.device.write_sectors(base_lba, buf, sectors)?;

        self.delete_long_name_sequence(
            parent_cluster,
            dir_cluster,
            entry_off,
            entry.short_name_checksum(),
        )?;

        Ok(())
    }

    fn remove_dir(&mut self, path: &Path) -> Result<(), super::Error> {
        // Reject root
        if path.normalize().is_root() {
            return Err(Error::NotADir);
        }

        let (parent_cluster, _) = self.resolve_parent_and_name(path)?;

        // Locate the entry and its position in parent directory
        let (entry, dir_cluster, entry_off) = match self.find_dir_entry(path)? {
            Some(x) => x,
            None => return Err(Error::FileNotFound),
        };
        if !entry.is_directory() {
            return Err(Error::NotADir);
        }

        let mut read_buffer = Vec::new();

        // Ensure the directory is empty (only "." and ".." allowed)
        let mut cur = entry.first_cluster();
        if cur >= 2 {
            let spc = self.boot_info.sectors_per_cluster as u16;
            loop {
                let base_lba = self.cluster_to_lba(cur);
                read_buffer.clear();
                read_buffer = self.device.read_sectors(base_lba, spc, read_buffer)?;
                let mut off = 0usize;
                while off + 32 <= read_buffer.len() {
                    let first = read_buffer[off];
                    if first == 0x00 {
                        break;
                    } // end marker
                    if first == 0xE5 {
                        off += 32;
                        continue;
                    }
                    let attr = read_buffer[off + 11];
                    if attr == ATTR_LONG_NAME {
                        off += 32;
                        continue;
                    }
                    let de: DirectoryEntry = *bytemuck::from_bytes(&read_buffer[off..off + 32]);
                    let name = de.fat_name_to_string();
                    if !name.eq(".") && !name.eq("..") {
                        return Err(Error::IoError); // not empty
                    }
                    off += 32;
                }
                match self.get_fat_entry(cur)? {
                    Some(next) => cur = next,
                    None => break,
                }
            }
        }

        // Free the directory's cluster chain
        let start = entry.first_cluster();
        if start >= 2 {
            let _freed = self.free_cluster_chain(start)?;
        }

        // Mark the parent directory entry deleted (0xE5)
        let (base_lba, sectors) = self.dir_entry_region(dir_cluster);

        read_buffer.clear();
        read_buffer = self.device.read_sectors(base_lba, sectors, read_buffer)?;
        if entry_off + 32 > read_buffer.len() {
            return Err(Error::IoError);
        }
        read_buffer[entry_off] = 0xE5;
        let _read_buffer = self.device.write_sectors(base_lba, read_buffer, sectors)?;

        self.delete_long_name_sequence(
            parent_cluster,
            dir_cluster,
            entry_off,
            entry.short_name_checksum(),
        )?;

        Ok(())
    }

    fn file_info(&mut self, path: &Path) -> Result<super::File, super::Error> {
        if let Some((entry, _, _)) = self.find_dir_entry(path)? {
            Ok(entry.into())
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn flush(&mut self) -> Result<(), Error> {
        // Only save FSInfo for FAT32
        if matches!(self.variant, FatVariant::Fat32) {
            self.save_fs_info()?;
        }
        self.device.flush()?;

        Ok(())
    }

    fn truncate(&mut self, path: &Path, size: u64) -> Result<(), Error> {
        let (entry, ec, eo) = match self.find_dir_entry(path)? {
            Some((e, c, o)) if !e.is_directory() => (e, c, o),
            Some(_) => return Err(Error::NotAFile),
            None => return Err(Error::FileNotFound),
        };

        if size == 0 {
            // Free all clusters if there are any
            let start = entry.first_cluster();
            if start >= 2 {
                self.free_cluster_chain(start)?;
            }
            // Zero out cluster pointer and file size in directory entry
            self.patch_dir_entry_at(ec, eo, |de| {
                de.file_size = 0;
                de.first_cluster_high = 0;
                de.first_cluster_low = 0;
            })?;
        } else {
            // For non-zero truncate, update the directory entry size.
            // Clusters beyond the new size are left allocated but inaccessible.
            self.patch_dir_entry_at(ec, eo, |de| {
                de.file_size = size as u32;
            })?;
        }

        Ok(())
    }

    fn rename(&mut self, old_path: &Path, new_path: &Path) -> Result<(), Error> {
        let old_path = old_path.normalize();
        let new_path = new_path.normalize();

        // Only support same-directory renames
        let old_parent = old_path.parent().ok_or(Error::IoError)?;
        let new_parent = new_path.parent().ok_or(Error::IoError)?;
        if old_parent != new_parent {
            return Err(Error::Unsupported);
        }

        let new_name = new_path.components().last().ok_or(Error::IoError)?.clone();

        let (entry, dir_cluster, entry_off) = match self.find_dir_entry(&old_path)? {
            Some(x) => x,
            None => return Err(Error::FileNotFound),
        };

        let (short_name, needs_lfn) = self.generate_short_name(dir_cluster, &new_name)?;

        // Delete old LFN entries before rewriting the short entry with the new name
        let checksum = entry.short_name_checksum();
        let (parent_cluster, _) = self.resolve_parent_and_name(&old_path)?;
        self.delete_long_name_sequence(parent_cluster, dir_cluster, entry_off, checksum)?;

        // Patch the short directory entry with the new name
        self.patch_dir_entry_at(dir_cluster, entry_off, |de| {
            de.set_name_from_string(&short_name);
        })?;

        // If the new name requires LFN entries, append them.
        // For simplicity, only the short name rename is fully supported here.
        // LFN creation on rename is not yet implemented.
        let _ = needs_lfn;

        Ok(())
    }

    fn statfs(&mut self) -> Result<super::StatFs, Error> {
        let bs = &self.boot_info;
        let cluster_size = bs.bytes_per_sector as u64 * bs.sectors_per_cluster as u64;
        let total_clusters = bs.calculate_cluster_count() as u64;
        let total_size = total_clusters * cluster_size;
        let total_blocks = total_size / cluster_size;

        let free_clusters = match &self.fs_info {
            Some(fi) if fi.has_free_count() => fi.free_count as u64,
            _ => 0,
        };

        let variant_name = match self.variant {
            FatVariant::Fat12 => "fat12",
            FatVariant::Fat16 => "fat16",
            FatVariant::Fat32 => "fat32",
        };

        let mut volume_name = [0u8; 64];
        let label = &bs.volume_label;
        let label_len = label
            .iter()
            .rposition(|&b| b != b' ' && b != 0)
            .map(|i| i + 1)
            .unwrap_or(0);
        volume_name[..label_len].copy_from_slice(&label[..label_len]);

        Ok(super::StatFs {
            fs_type: variant_name,
            block_size: cluster_size,
            total_blocks,
            free_blocks: free_clusters,
            total_inodes: 0,
            free_inodes: 0,
            volume_name,
            volume_name_len: label_len,
            version: 0,
            block_groups: 0,
        })
    }
}
