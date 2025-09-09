use alloc::vec::Vec;
use bytemuck::bytes_of;

use crate::{
    drivers::ahci::api::{read_sectors, write_sectors},
    fs::{
        Error,
        fat32::{
            Fat32fs,
            structures::{CLUSTER_FREE, DirectoryEntry},
        },
        path::Path,
    },
    println,
};

impl Fat32fs {
    /// Overwrite file contents starting at offset 0.
    /// Extends the cluster chain if `buf` is larger than the current file.
    /// Does not shrink or update the on-disk directory entry size yet.
    pub fn write_file(
        &mut self,
        entry: &mut DirectoryEntry,
        entry_pos: Option<(u32, usize)>,
        buf: &[u8],
    ) -> Result<usize, Error> {
        self.write_file_offset(entry, entry_pos, 0, buf)
    }

    /// Write `buf` at byte `offset` into `entry`.
    /// Allocates new clusters when writing past the current chain end.
    /// Returns bytes written.
    pub fn write_file_offset(
        &mut self,
        entry: &mut crate::fs::fat32::structures::DirectoryEntry,
        entry_pos: Option<(u32, usize)>, // needed only if entry.first_cluster()<2
        offset: u64,
        buf: &[u8],
    ) -> Result<usize, Error> {
        use crate::drivers::ahci::api::{read_sectors, write_sectors};
        use crate::fs::fat32::structures::DirectoryEntry;

        if entry.is_directory() {
            return Err(Error::NotAFile);
        }
        if buf.is_empty() {
            return Ok(0);
        }

        let bps = self.boot_info.bytes_per_sector as usize;
        let spc = self.boot_info.sectors_per_cluster as usize;
        let bpc = bps * spc;
        let spc_u16 = spc as u16;

        let target_idx = (offset as usize) / bpc;
        let mut head = entry.first_cluster();
        let mut fresh_current = false;

        // Ensure a chain exists up to target_idx
        if head < 2 {
            // Need head cluster; must have position to patch on-disk entry.
            let (dir_cluster, entry_off) = entry_pos.ok_or(Error::IoError)?;
            head = self.alloc_cluster()?;
            let mut last = head;
            for _ in 0..target_idx {
                let nc = self.alloc_cluster()?;
                self.link_fat_entry(last, nc)?;
                last = nc;
            }

            // Patch on-disk entry with new head
            let base_lba = self.cluster_to_lba(dir_cluster);
            let mut dirbuf = read_sectors(self.partition.device_id, base_lba, spc_u16)?;
            if entry_off + 32 > dirbuf.len() {
                return Err(Error::IoError);
            }
            let mut de: DirectoryEntry = *bytemuck::from_bytes(&dirbuf[entry_off..entry_off + 32]);
            de.first_cluster_high = ((head >> 16) & 0xFFFF) as u16;
            de.first_cluster_low = (head & 0xFFFF) as u16;
            let bytes: [u8; 32] = bytemuck::cast(de);
            dirbuf[entry_off..entry_off + 32].copy_from_slice(&bytes);
            write_sectors(self.partition.device_id, base_lba, dirbuf, spc_u16)?;

            // Update in-memory copy
            entry.first_cluster_high = ((head >> 16) & 0xFFFF) as u16;
            entry.first_cluster_low = (head & 0xFFFF) as u16;

            fresh_current = true; // first target cluster is new if target_idx==0; handled again below
        } else {
            // Walk/extend chain up to target_idx
            let mut idx = 0usize;
            let mut cur = head;
            let mut last = head;
            while idx < target_idx {
                match self.get_fat_entry(cur)? {
                    Some(n) => {
                        last = n;
                        cur = n;
                        idx += 1;
                    }
                    None => {
                        let nc = self.alloc_cluster()?;
                        self.link_fat_entry(last, nc)?;
                        last = nc;
                        cur = nc;
                        idx += 1;
                        if idx == target_idx {
                            fresh_current = true;
                        } // target cluster newly allocated
                    }
                }
            }
            if !fresh_current {
                // Infer freshness from old size if no allocation happened above
                let old_clusters = if entry.file_size == 0 {
                    0
                } else {
                    (entry.file_size as usize).div_ceil(bpc)
                };
                fresh_current = target_idx >= old_clusters;
            }
        }

        // Position at target cluster
        let mut cluster = head;
        let mut skip = target_idx;
        while skip > 0 {
            cluster = self.get_fat_entry(cluster)?.ok_or(Error::IoError)?;
            skip -= 1;
        }
        let mut inner_off = (offset as usize) % bpc;

        // Write loop
        let mut wrote = 0usize;
        let mut remaining = buf.len();
        let mut buf_off = 0usize;

        loop {
            let lba = self.cluster_to_lba(cluster);

            // Skip read for freshly allocated clusters
            let mut scratch = if fresh_current {
                alloc::vec![0u8; bpc]
            } else {
                let v = read_sectors(self.partition.device_id, lba, spc_u16)?;
                if v.len() != bpc {
                    return Err(Error::IoError);
                }
                v
            };

            let space = bpc - inner_off;
            let take = space.min(remaining);
            scratch[inner_off..inner_off + take].copy_from_slice(&buf[buf_off..buf_off + take]);

            write_sectors(self.partition.device_id, lba, scratch, spc_u16)?;

            wrote += take;
            remaining -= take;
            buf_off += take;
            inner_off = 0;
            fresh_current = false; // only the first of the pair can be fresh here

            if remaining == 0 {
                break;
            }

            // Advance to next cluster, allocate if at EOF
            match self.get_fat_entry(cluster)? {
                Some(next) => {
                    cluster = next;
                    // fresh_current remains false unless we allocate
                }
                None => {
                    let nc = self.alloc_cluster()?;
                    self.link_fat_entry(cluster, nc)?;
                    cluster = nc;
                    fresh_current = true; // next iteration writes a new cluster → skip read
                }
            }
        }

        Ok(wrote)
    }

    /// Allocate a new free cluster and mark it EOF in FAT.
    pub fn alloc_cluster(&mut self) -> Result<u32, Error> {
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
    pub fn link_fat_entry(&self, from: u32, to: u32) -> Result<(), Error> {
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

    /// Resolve parent directory cluster and final component name.
    pub fn resolve_parent_and_name(
        &self,
        path: &Path,
    ) -> Result<(u32, alloc::string::String), Error> {
        let path = path.normalize();
        let comps = path.components();
        if comps.is_empty() {
            return Err(Error::NotADir); // cannot create at root without a name
        }
        let parent_components = &comps[..comps.len() - 1];
        let name = comps[comps.len() - 1].clone();

        // Walk down from root
        let mut dir_cluster = self.boot_info.root_cluster;
        for comp in parent_components {
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
                None => return Err(Error::FileNotFound),
            };
            dir_cluster = de.first_cluster();
        }

        Ok((dir_cluster, name))
    }

    /// Append a short 8.3 DirectoryEntry to a directory, allocating a new cluster if needed.
    /// Returns (cluster_that_contains_entry, byte_offset_within_cluster).
    pub fn append_dir_entry(
        &mut self,
        mut dir_cluster: u32,
        new_entry: &DirectoryEntry,
    ) -> Result<(u32, usize), Error> {
        let spc = self.boot_info.sectors_per_cluster as u16;
        let bps = self.boot_info.bytes_per_sector as usize;
        let cluster_bytes = bps * spc as usize;

        loop {
            let base_lba = self.cluster_to_lba(dir_cluster);
            let mut buf = read_sectors(self.partition.device_id, base_lba, spc)?;
            if buf.len() < cluster_bytes {
                return Err(Error::IoError);
            }

            // Scan for free slot (0xE5 = deleted, 0x00 = end/free)
            let mut off = 0usize;
            while off + 32 <= buf.len() {
                let first = buf[off];
                if first == 0x00 || first == 0xE5 {
                    let bytes: [u8; 32] = bytemuck::cast(*new_entry);
                    buf[off..off + 32].copy_from_slice(&bytes);
                    write_sectors(self.partition.device_id, base_lba, buf, spc)?;
                    return Ok((dir_cluster, off));
                }
                off += 32;
            }

            // No space here. Advance or extend directory chain.
            match self.get_fat_entry(dir_cluster)? {
                Some(next) => dir_cluster = next,
                None => {
                    let newc = self.alloc_cluster()?;
                    self.link_fat_entry(dir_cluster, newc)?;
                    // Zero-fill new cluster and write the entry at offset 0
                    let zero = alloc::vec![0u8; cluster_bytes];
                    write_sectors(
                        self.partition.device_id,
                        self.cluster_to_lba(newc),
                        zero,
                        spc,
                    )?;
                    let mut buf =
                        read_sectors(self.partition.device_id, self.cluster_to_lba(newc), spc)?;
                    let bytes: [u8; 32] = bytemuck::cast(*new_entry);
                    buf[0..32].copy_from_slice(&bytes);
                    write_sectors(
                        self.partition.device_id,
                        self.cluster_to_lba(newc),
                        buf,
                        spc,
                    )?;
                    return Ok((newc, 0));
                }
            }
        }
    }

    pub(super) fn free_cluster_chain(&mut self, start: u32) -> Result<u32, Error> {
        if start < 2 {
            return Ok(0);
        }

        // Hard guard to avoid infinite loops on corrupted chains
        let fat_entries =
            (self.boot_info.fat_size_32 as usize * self.boot_info.bytes_per_sector as usize) / 4;

        let mut freed: u32 = 0;
        let mut cur = start;
        let mut visited = 0usize;

        loop {
            visited += 1;
            if visited > fat_entries {
                // cycle or corruption
                println!("Possible corruption");
                return Err(Error::IoError);
            }

            // Read next before clearing current
            let next = self.get_fat_entry(cur)?; // None => EOF

            // Mark current as free in both FATs
            self.set_fat_value(cur, CLUSTER_FREE)?;
            freed = freed.saturating_add(1);

            match next {
                Some(n) => cur = n,
                None => break,
            }
        }

        // Update FSInfo in memory
        if self.fs_info.free_count != 0xFFFF_FFFF {
            self.fs_info.free_count = self.fs_info.free_count.saturating_add(freed);
        }
        if self.fs_info.next_free == 0xFFFF_FFFF || start < self.fs_info.next_free {
            self.fs_info.next_free = start;
        }

        Ok(freed)
    }

    /// Copy the entire primary FAT to the backup FAT.
    pub fn mirror_primary_fat_to_backup(&self) -> Result<(), Error> {
        let dev = self.partition.device_id;
        let total: u64 = self.boot_info.fat_size_32 as u64; // sectors in one FAT
        let mut src_lba = self.first_fat_lba();
        let mut dst_lba = self.backup_fat_lba();

        // Copy in chunks to avoid large allocations
        let chunk: u16 = 64; // sectors per transfer
        let mut remaining = total;

        while remaining > 0 {
            let take = core::cmp::min(remaining, chunk as u64) as u16;
            let buf = read_sectors(dev, src_lba, take)?;
            write_sectors(dev, dst_lba, buf, take)?;

            src_lba += take as u64;
            dst_lba += take as u64;
            remaining -= take as u64;
        }

        Ok(())
    }
}
