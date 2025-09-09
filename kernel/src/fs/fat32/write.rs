use alloc::vec::Vec;
use bytemuck::bytes_of;

use crate::{
    drivers::ahci::api::{read_sectors, write_sectors},
    fs::{
        Error,
        fat32::{Fat32fs, structures::DirectoryEntry},
    },
};

impl Fat32fs {
    /// Overwrite file contents starting at offset 0.
    /// Extends the cluster chain if `buf` is larger than the current file.
    /// Does not shrink or update the on-disk directory entry size yet.
    pub fn write_file(&mut self, entry: &DirectoryEntry, buf: &[u8]) -> Result<usize, Error> {
        self.write_file_offset(entry, 0, buf)
    }

    /// Write `buf` at byte `offset` into `entry`.
    /// Allocates new clusters when writing past the current chain end.
    /// Returns bytes written.
    pub fn write_file_offset(
        &mut self,
        entry: &DirectoryEntry,
        mut offset: u64,
        buf: &[u8],
    ) -> Result<usize, Error> {
        if entry.is_directory() {
            return Err(Error::NotAFile);
        }
        if buf.is_empty() {
            return Ok(0);
        }

        // Geometry
        let bytes_per_sector = self.boot_info.bytes_per_sector as usize;
        let sectors_per_cluster = self.boot_info.sectors_per_cluster as usize;
        let bytes_per_cluster = bytes_per_sector * sectors_per_cluster;

        // Navigate to the cluster that contains `offset`
        let mut cluster = entry.first_cluster();
        let mut inner_off = (offset % bytes_per_cluster as u64) as usize;
        let mut clusters_to_skip = (offset / bytes_per_cluster as u64) as usize;
        while clusters_to_skip > 0 {
            match self.get_fat_entry(cluster)? {
                Some(next) => {
                    cluster = next;
                    clusters_to_skip -= 1;
                }
                None => {
                    // Need to allocate clusters to reach the desired offset
                    while clusters_to_skip > 0 {
                        let newc = self.alloc_cluster()?;
                        self.link_fat_entry(cluster, newc)?;
                        cluster = newc;
                        clusters_to_skip -= 1;
                    }
                    break;
                }
            }
        }

        // Write loop
        let mut wrote = 0usize;
        let mut remaining = buf.len();
        let mut buf_off = 0usize;

        loop {
            // Ensure current cluster exists
            // For partial writes, do read-modify-write of the cluster
            // Build a cluster-sized scratch buffer
            let mut scratch = alloc::vec![0; bytes_per_cluster];

            // Read existing cluster to preserve unaffected bytes
            let lba = self.cluster_to_lba(cluster);
            read_sectors(self.partition.device_id, lba, sectors_per_cluster as u16)?;

            let space = bytes_per_cluster - inner_off;
            let take = space.min(remaining);

            // Copy user data into scratch at inner_off
            scratch[inner_off..inner_off + take].copy_from_slice(&buf[buf_off..buf_off + take]);

            // Write cluster back
            write_sectors(
                self.partition.device_id,
                lba,
                scratch,
                sectors_per_cluster as u16,
            )?;

            wrote += take;
            remaining -= take;
            buf_off += take;
            inner_off = 0;

            if remaining == 0 {
                break;
            }

            // Advance to next cluster, allocate if needed
            match self.get_fat_entry(cluster)? {
                Some(next) => cluster = next,
                None => {
                    let newc = self.alloc_cluster()?;
                    self.link_fat_entry(cluster, newc)?;
                    cluster = newc;
                }
            }
        }

        // Note: updating the directory entry's file_size and timestamps
        // should be done by the caller or via a dedicated metadata updater.
        Ok(wrote)
    }

    /// Allocate a new free cluster and mark it EOF in FAT.
    fn alloc_cluster(&mut self) -> Result<u32, Error> {
        let bytes_per_sector = self.boot_info.bytes_per_sector as usize;
        let fat_sectors = self.boot_info.fat_size_32 as u64;
        let entries_per_sector = bytes_per_sector / 4;

        // Start hint from FSInfo if valid, else 2
        let mut start_cluster = if self.fs_info.next_free != 0xFFFF_FFFF {
            self.fs_info.next_free
        } else {
            2
        };
        if start_cluster < 2 {
            start_cluster = 2;
        }

        // Search helper
        let mut search = |start: u32, _end_exclusive: u32| -> Option<u32> {
            let mut current_cluster = start;
            while (current_cluster as u64) < (entries_per_sector as u64 * fat_sectors) {
                // Compute sector and offset
                let byte_index = (current_cluster as usize) * 4;
                let sector_index = (byte_index / bytes_per_sector) as u64;
                let within = byte_index % bytes_per_sector;

                // Read FAT sector
                let mut sec = crate::drivers::ahci::api::read_sectors(
                    self.partition.device_id,
                    self.first_fat_lba() + sector_index,
                    1,
                )
                .ok()?;

                let val = u32::from_le_bytes([
                    sec[within],
                    sec[within + 1],
                    sec[within + 2],
                    sec[within + 3],
                ]) & crate::fs::fat32::structures::FAT32_MASK;

                if val == crate::fs::fat32::structures::CLUSTER_FREE {
                    // mark as EOF in both FATs
                    let newv = crate::fs::fat32::structures::CLUSTER_EOF;
                    sec[within..within + 4].copy_from_slice(&newv.to_le_bytes());

                    crate::drivers::ahci::api::write_sectors(
                        self.partition.device_id,
                        self.first_fat_lba() + sector_index,
                        sec.clone(),
                        1,
                    )
                    .ok()?;

                    // Mirror to backup FAT
                    crate::drivers::ahci::api::write_sectors(
                        self.partition.device_id,
                        self.backup_fat_lba() + sector_index,
                        sec,
                        1,
                    )
                    .ok()?;

                    let next = if current_cluster == u32::MAX {
                        2
                    } else {
                        current_cluster.saturating_add(1).max(2)
                    };
                    self.fs_info.next_free = next;
                    if self.fs_info.free_count != 0xFFFF_FFFF && self.fs_info.free_count > 0 {
                        self.fs_info.free_count -= 1;
                    }

                    return Some(current_cluster);
                }
                current_cluster += 1;
            }
            None
        };

        // First pass from hint
        if let Some(c) = search(
            start_cluster,
            (entries_per_sector as u32 * fat_sectors as u32),
        ) {
            return Ok(c);
        }
        // Second pass from 2
        if let Some(c) = search(2, start_cluster) {
            return Ok(c);
        }

        Err(Error::IoError)
    }

    /// Link `from` cluster to `to` cluster by setting FAT[from] = to,
    /// and set FAT[to] to EOF.
    fn link_fat_entry(&self, from: u32, to: u32) -> Result<(), Error> {
        self.set_fat_value(from, to)?;
        self.set_fat_value(to, crate::fs::fat32::structures::CLUSTER_EOF)?;
        Ok(())
    }

    /// Low-level setter for a FAT entry. Writes to both primary and backup FATs.
    fn set_fat_value(&self, cluster: u32, value: u32) -> Result<(), Error> {
        let bytes_per_sector = self.boot_info.bytes_per_sector as usize;
        let byte_index = (cluster as usize) * 4;
        let sector_index = (byte_index / bytes_per_sector) as u64;
        let within = byte_index % bytes_per_sector;

        // Update primary
        let mut sec = crate::drivers::ahci::api::read_sectors(
            self.partition.device_id,
            self.first_fat_lba() + sector_index,
            1,
        )?;
        let v = value & crate::fs::fat32::structures::FAT32_MASK;
        sec[within..within + 4].copy_from_slice(&v.to_le_bytes());
        crate::drivers::ahci::api::write_sectors(
            self.partition.device_id,
            self.first_fat_lba() + sector_index,
            sec.clone(),
            1,
        )?;

        // Mirror to backup
        crate::drivers::ahci::api::write_sectors(
            self.partition.device_id,
            self.backup_fat_lba() + sector_index,
            sec,
            1,
        )?;

        Ok(())
    }

    pub fn save_fs_info(&self) -> Result<(), Error> {
        let data: Vec<u8> = bytes_of(&self.fs_info).to_vec(); // 512 bytes
        crate::drivers::ahci::api::write_sectors(
            self.partition.device_id,
            self.partition.starting_lba + self.boot_info.fs_info as u64,
            data,
            1,
        )?;
        Ok(())
    }

    pub fn patch_dir_entry_at(
        &self,
        entry_cluster: u32,
        entry_offset: usize, // byte offset within the cluster
        patch: impl FnOnce(&mut DirectoryEntry),
    ) -> Result<(), Error> {
        let spc = self.boot_info.sectors_per_cluster as u16;
        let bps = self.boot_info.bytes_per_sector as usize;
        let cluster_bytes = bps * spc as usize;

        let base_lba = self.cluster_to_lba(entry_cluster);
        let mut buf = read_sectors(self.partition.device_id, base_lba, spc)?;
        if buf.len() < cluster_bytes || entry_offset + 32 > buf.len() {
            return Err(Error::IoError);
        }

        let mut de: DirectoryEntry = *bytemuck::from_bytes(&buf[entry_offset..entry_offset + 32]);
        patch(&mut de);
        let bytes: [u8; 32] = bytemuck::cast(de);
        buf[entry_offset..entry_offset + 32].copy_from_slice(&bytes);

        write_sectors(self.partition.device_id, base_lba, buf, spc)?;
        Ok(())
    }
}
