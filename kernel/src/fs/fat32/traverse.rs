use alloc::vec::Vec;
use bytemuck::cast;

use crate::{
    drivers::ahci::api::read_sectors,
    fs::{
        Error,
        fat32::{
            Fat32fs,
            structures::{CLUSTER_BAD, CLUSTER_EOF, CLUSTER_FREE, DirectoryEntry, FAT32_MASK},
        },
        path::Path,
    },
};

impl Fat32fs {
    /// Find a directory entry by absolute path.
    /// Returns `Ok(None)` if not found. Root ("/") has no dir entry → returns `Ok(None)`.
    pub fn find_dir_entry(&self, path: &Path) -> Result<Option<DirectoryEntry>, Error> {
        let path = path.normalize();

        // Start at FAT32 root directory cluster
        let mut dir_cluster = self.boot_info.root_cluster;

        // Build clean component list: absolute, no empty parts, no "/"
        let comps_src = path.components();
        let mut comps: Vec<&str> = Vec::new();
        for c in comps_src {
            if c.is_empty() || c == "/" {
                continue;
            }
            comps.push(c.as_str());
        }

        // Edge: path is "/" → no dir entry exists for root
        if comps.is_empty() {
            return Ok(None);
        }

        // Walk components
        let last_idx = comps.len() - 1;
        let mut found_entry: Option<DirectoryEntry> = None;

        for (i, name) in comps.iter().enumerate() {
            // List current directory
            let entries = self.get_dir_entries(dir_cluster)?;

            // Linear search by short name (8.3). Case-insensitive per FAT.
            let mut hit: Option<DirectoryEntry> = None;
            for e in entries {
                // Skip LFN entries if your DirectoryEntry represents only short entries.
                // If your DirectoryEntry includes LFNs, add handling here.
                let cand = e.fat_name_to_string(); // e.g., "FOO.TXT"
                if cand.eq_ignore_ascii_case(name) {
                    hit = Some(e);
                    break;
                }
            }

            // Not found in this directory
            match hit {
                None => return Ok(None),
                Some(e) => {
                    let is_last = i == last_idx;
                    if is_last {
                        found_entry = Some(e);
                    } else {
                        // Must descend into a directory
                        if !e.is_directory() {
                            return Ok(None);
                        }
                        dir_cluster = e.first_cluster();
                    }
                }
            }
        }

        Ok(found_entry)
    }

    pub fn get_dir_entries(&self, start_cluster: u32) -> Result<Vec<DirectoryEntry>, Error> {
        let mut cluster = start_cluster;

        let mut entries = Vec::new();

        loop {
            let base_lba = self.cluster_to_lba(cluster);

            let data = read_sectors(
                self.partition.device_id,
                base_lba,
                self.boot_info.sectors_per_cluster as u16,
            )?;

            let mut offset = 0;

            while offset + 32 < data.len() {
                let entry_slice = &data[offset..offset + 32];
                let first = entry_slice[0];

                if first == 0x00 {
                    // no more entries
                    return Ok(entries);
                }

                if first == 0xE5 {
                    offset += 32;
                    continue; // deleted entry
                }

                let entry: DirectoryEntry = *bytemuck::from_bytes(entry_slice);

                if !entry.is_volume_label() {
                    entries.push(entry);
                }

                offset += 32;
            }

            if let Some(next) = self.get_fat_entry(cluster)? {
                cluster = next;
            }
        }
    }

    pub fn get_fat_entry(&self, cluster_number: u32) -> Result<Option<u32>, Error> {
        let byte_off = (cluster_number as u64) * 4;
        let fat_sector = self.first_fat_lba() + (byte_off / 512);
        let off_in_sector = (byte_off % 512) as usize;

        let sector = read_sectors(self.partition.device_id, fat_sector, 1)?;

        let raw = u32::from_le_bytes(sector[off_in_sector..off_in_sector + 4].try_into().unwrap());
        let val = raw & FAT32_MASK;

        if val == CLUSTER_FREE || val >= CLUSTER_EOF {
            return Ok(None);
        }

        if val == CLUSTER_BAD {
            return Err(Error::IoError);
        }

        Ok(Some(raw))
    }

    /// Convert a cluster number (>=2) into an absolute LBA.
    #[inline(always)]
    pub fn cluster_to_lba(&self, cluster: u32) -> u64 {
        let reserved = self.boot_info.reserved_sector_count as u64;
        let fatsz = self.boot_info.fat_size_32 as u64;
        let numfats = self.boot_info.num_fats as u64;
        let spc = self.boot_info.sectors_per_cluster as u64;
        let start = self.partition.starting_lba;

        // First sector of data region relative to partition start
        let first_data_sector = reserved + (numfats * fatsz);

        // Absolute LBA
        start + ((cluster as u64 - 2) * spc) + first_data_sector
    }

    #[inline(always)]
    fn first_fat_lba(&self) -> u64 {
        self.partition.starting_lba + self.boot_info.reserved_sector_count as u64
    }

    #[inline(always)]
    fn backup_fat_lba(&self) -> u64 {
        self.partition.starting_lba
            + self.boot_info.reserved_sector_count as u64
            + self.boot_info.fat_size_32 as u64
    }

    #[inline(always)]
    fn first_data_lba(&self) -> u64 {
        self.partition.starting_lba
            + self.boot_info.reserved_sector_count as u64
            + (self.boot_info.num_fats as u64 * self.boot_info.fat_size_32 as u64)
    }
}
