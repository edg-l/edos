use alloc::vec::Vec;
use bytemuck::cast;

use crate::fs::{
    Error,
    fat32::{
        Fat32fs,
        structures::{CLUSTER_BAD, CLUSTER_EOF, CLUSTER_FREE, DirectoryEntry, FAT32_MASK},
    },
    path::Path,
};

impl Fat32fs {
    /// Find entry and return (entry, cluster_that_contains_it, byte_offset_within_cluster).
    pub fn find_dir_entry(
        &self,
        path: &crate::fs::path::Path,
    ) -> Result<Option<(DirectoryEntry, u32, usize)>, Error> {
        // Resolve parent directory cluster chain
        let path = path.normalize();
        let comps = path.components();
        if comps.is_empty() {
            return Ok(None); // root has no entry
        }
        let (parent, name) = (&comps[..comps.len() - 1], &comps[comps.len() - 1]);

        let mut dir_cluster = self.boot_info.root_cluster;
        for comp in parent {
            let entries = self.get_dir_entries(dir_cluster)?;
            let mut hit = None;
            for e in entries {
                if e.is_directory() && e.fat_name_to_string().eq_ignore_ascii_case(comp) {
                    hit = Some(e);
                    break;
                }
            }
            let de = match hit {
                Some(x) => x,
                None => return Ok(None),
            };
            dir_cluster = de.first_cluster();
        }

        // Scan clusters and return exact position for `name`
        let spc = self.boot_info.sectors_per_cluster as u16;
        let bps = self.boot_info.bytes_per_sector as usize;
        let cluster_bytes = bps * spc as usize;
        let mut cur = dir_cluster;

        let mut buf = Vec::new();
        loop {
            let base_lba = self.cluster_to_lba(cur);
            buf.clear();
            buf = self.device.read_sectors(base_lba, spc, buf)?;
            let mut off = 0usize;

            while off + 32 <= buf.len() {
                let first = buf[off];
                if first == 0x00 {
                    return Ok(None);
                }
                if first != 0xE5 {
                    let de: DirectoryEntry = *bytemuck::from_bytes(&buf[off..off + 32]);
                    if de.fat_name_to_string().eq_ignore_ascii_case(name) {
                        return Ok(Some((de, cur, off)));
                    }
                }
                off += 32;
            }

            match self.get_fat_entry(cur)? {
                Some(next) => cur = next,
                None => return Ok(None),
            }
        }
    }

    pub fn get_dir_entries(&self, start_cluster: u32) -> Result<Vec<DirectoryEntry>, Error> {
        let mut cluster = start_cluster;

        let mut entries = Vec::new();

        let mut data = Vec::new();
        loop {
            let base_lba = self.cluster_to_lba(cluster);
            data.clear();
            data = self.device.read_sectors(
                base_lba,
                self.boot_info.sectors_per_cluster as u16,
                data,
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

        let sector = self.device.read_sectors(fat_sector, 1, Vec::new())?;

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
    pub fn first_fat_lba(&self) -> u64 {
        self.partition.starting_lba + self.boot_info.reserved_sector_count as u64
    }

    #[inline(always)]
    pub fn backup_fat_lba(&self) -> u64 {
        self.partition.starting_lba
            + self.boot_info.reserved_sector_count as u64
            + self.boot_info.fat_size_32 as u64
    }

    #[inline(always)]
    pub fn first_data_lba(&self) -> u64 {
        self.partition.starting_lba
            + self.boot_info.reserved_sector_count as u64
            + (self.boot_info.num_fats as u64 * self.boot_info.fat_size_32 as u64)
    }
}
