use alloc::vec::Vec;

use crate::fs::{
    Error,
    fat32::{
        Fatfs,
        structures::{
            ATTR_LONG_NAME, CLUSTER_BAD, CLUSTER_EOF, CLUSTER_FREE, DirectoryEntry,
            DirectoryRecord, FAT16_MASK, FAT32_MASK, FatVariant, LongFilenameEntry,
            decode_long_name,
        },
    },
};

impl Fatfs {
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

        // Start from root - handle FAT12/16 vs FAT32 differently
        let mut dir_cluster = match self.variant {
            FatVariant::Fat32 => self.boot_info.root_cluster,
            FatVariant::Fat12 | FatVariant::Fat16 => 0, // Use 0 as special marker for root
        };

        for comp in parent {
            let entries = if dir_cluster == 0
                && matches!(self.variant, FatVariant::Fat12 | FatVariant::Fat16)
            {
                self.get_root_dir_entries()?
            } else {
                self.get_dir_entries(dir_cluster)?
            };

            let mut hit = None;
            for record in entries {
                if record.entry.is_directory() && record.matches_name(comp) {
                    hit = Some(record.entry);
                    break;
                }
            }
            let de = match hit {
                Some(x) => x,
                None => return Ok(None),
            };
            dir_cluster = de.first_cluster();
        }

        // Scan for the final entry - handle FAT12/16 root vs cluster-based directories
        if dir_cluster == 0 && matches!(self.variant, FatVariant::Fat12 | FatVariant::Fat16) {
            // Search in FAT12/16 root directory
            let root_dir_lba = self.root_dir_lba();
            let root_dir_sectors = (self.boot_info.root_entry_count as u64 * 32).div_ceil(512);

            let mut buf = Vec::new();
            let mut lfn_stack: Vec<LongFilenameEntry> = Vec::new();
            for sector in 0..root_dir_sectors {
                buf.clear();
                buf = self.device.read_sectors(root_dir_lba + sector, 1, buf)?;
                let mut off = 0usize;

                while off + 32 <= buf.len() {
                    let slice = &buf[off..off + 32];
                    let first = slice[0];
                    if first == 0x00 {
                        return Ok(None);
                    }
                    if first == 0xE5 {
                        lfn_stack.clear();
                        off += 32;
                        continue;
                    }

                    let attr = slice[11];
                    if attr == ATTR_LONG_NAME {
                        let lfn: LongFilenameEntry = *bytemuck::from_bytes(slice);
                        if lfn.order & 0x40 != 0 {
                            lfn_stack.clear();
                        }
                        lfn_stack.push(lfn);
                        off += 32;
                        continue;
                    }

                    let de: DirectoryEntry = *bytemuck::from_bytes(slice);
                    if de.is_volume_label() {
                        lfn_stack.clear();
                        off += 32;
                        continue;
                    }

                    let long_name = if !lfn_stack.is_empty()
                        && lfn_stack
                            .first()
                            .is_some_and(|lfn| (lfn.order & 0x1F) as usize == lfn_stack.len())
                        && lfn_stack
                            .last()
                            .is_some_and(|lfn| lfn.checksum == de.short_name_checksum())
                    {
                        let name = decode_long_name(&lfn_stack);
                        lfn_stack.clear();
                        name
                    } else {
                        lfn_stack.clear();
                        None
                    };

                    let short_name = de.fat_name_to_string();
                    let matches = if let Some(long) = &long_name {
                        long == name || long.eq_ignore_ascii_case(name)
                    } else {
                        false
                    } || short_name.eq_ignore_ascii_case(name)
                        || short_name == *name;

                    if matches {
                        // Return sentinel cluster 0 for FAT12/16 root dir.
                        // Offset is absolute within the root dir region.
                        let abs_off = sector as usize * 512 + off;
                        return Ok(Some((de, 0, abs_off)));
                    }

                    off += 32;
                }
            }
            Ok(None)
        } else {
            // Search in cluster-based directory (FAT32 root or any subdirectory)
            let spc = self.boot_info.sectors_per_cluster as u16;
            let mut cur = dir_cluster;

            let mut buf = Vec::new();
            let mut lfn_stack: Vec<LongFilenameEntry> = Vec::new();
            loop {
                let base_lba = self.cluster_to_lba(cur);
                buf.clear();
                buf = self.device.read_sectors(base_lba, spc, buf)?;
                let mut off = 0usize;

                while off + 32 <= buf.len() {
                    let slice = &buf[off..off + 32];
                    let first = slice[0];
                    if first == 0x00 {
                        return Ok(None);
                    }
                    if first == 0xE5 {
                        lfn_stack.clear();
                        off += 32;
                        continue;
                    }

                    let attr = slice[11];
                    if attr == ATTR_LONG_NAME {
                        let lfn: LongFilenameEntry = *bytemuck::from_bytes(slice);
                        if lfn.order & 0x40 != 0 {
                            lfn_stack.clear();
                        }
                        lfn_stack.push(lfn);
                        off += 32;
                        continue;
                    }

                    let de: DirectoryEntry = *bytemuck::from_bytes(slice);
                    if de.is_volume_label() {
                        lfn_stack.clear();
                        off += 32;
                        continue;
                    }

                    let long_name = if !lfn_stack.is_empty()
                        && lfn_stack
                            .first()
                            .is_some_and(|lfn| (lfn.order & 0x1F) as usize == lfn_stack.len())
                        && lfn_stack
                            .last()
                            .is_some_and(|lfn| lfn.checksum == de.short_name_checksum())
                    {
                        let name = decode_long_name(&lfn_stack);
                        lfn_stack.clear();
                        name
                    } else {
                        lfn_stack.clear();
                        None
                    };

                    let short_name = de.fat_name_to_string();
                    let matches = if let Some(long) = &long_name {
                        long == name || long.eq_ignore_ascii_case(name)
                    } else {
                        false
                    } || short_name.eq_ignore_ascii_case(name)
                        || short_name == *name;

                    if matches {
                        return Ok(Some((de, cur, off)));
                    }

                    off += 32;
                }

                match self.get_fat_entry(cur)? {
                    Some(next) => cur = next,
                    None => return Ok(None),
                }
            }
        }
    }

    pub fn get_dir_entries(&self, start_cluster: u32) -> Result<Vec<DirectoryRecord>, Error> {
        let mut cluster = start_cluster;
        let mut entries = Vec::new();
        let mut lfn_stack: Vec<LongFilenameEntry> = Vec::new();

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

            while offset + 32 <= data.len() {
                let entry_slice = &data[offset..offset + 32];
                let first = entry_slice[0];

                if first == 0x00 {
                    return Ok(entries);
                }

                if first == 0xE5 {
                    lfn_stack.clear();
                    offset += 32;
                    continue;
                }

                let attr = entry_slice[11];
                if attr == ATTR_LONG_NAME {
                    let lfn: LongFilenameEntry = *bytemuck::from_bytes(entry_slice);
                    if lfn.order & 0x40 != 0 {
                        lfn_stack.clear();
                    }
                    lfn_stack.push(lfn);
                    offset += 32;
                    continue;
                }

                let entry: DirectoryEntry = *bytemuck::from_bytes(entry_slice);

                if entry.is_volume_label() {
                    lfn_stack.clear();
                    offset += 32;
                    continue;
                }

                let long_name = if !lfn_stack.is_empty()
                    && lfn_stack.iter().all(|lfn| lfn.attributes == ATTR_LONG_NAME)
                    && lfn_stack.first().is_some_and(|lfn| {
                        let count = (lfn.order & 0x1F) as usize;
                        let expected = lfn_stack.len();
                        count == expected
                    })
                    && lfn_stack
                        .last()
                        .is_some_and(|lfn| lfn.checksum == entry.short_name_checksum())
                {
                    let name = decode_long_name(&lfn_stack);
                    lfn_stack.clear();
                    name
                } else {
                    lfn_stack.clear();
                    None
                };

                let short_name = entry.fat_name_to_string();
                entries.push(DirectoryRecord {
                    entry,
                    long_name,
                    short_name,
                });

                offset += 32;
            }

            match self.get_fat_entry(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(entries),
            }
        }
    }

    pub fn get_fat_entry(&self, cluster_number: u32) -> Result<Option<u32>, Error> {
        match self.variant {
            FatVariant::Fat32 => {
                let byte_off = (cluster_number as u64) * 4;
                let fat_sector = self.first_fat_lba() + (byte_off / 512);
                let off_in_sector = (byte_off % 512) as usize;

                let sector = self.device.read_sectors(fat_sector, 1, Vec::new())?;

                let raw = u32::from_le_bytes(
                    sector[off_in_sector..off_in_sector + 4].try_into().unwrap(),
                );
                let val = raw & FAT32_MASK;

                if val == CLUSTER_FREE || val >= CLUSTER_EOF {
                    return Ok(None);
                }

                if val == CLUSTER_BAD {
                    return Err(Error::IoError);
                }

                Ok(Some(val))
            }
            FatVariant::Fat16 => {
                let byte_off = (cluster_number as u64) * 2;
                let fat_sector = self.first_fat_lba() + (byte_off / 512);
                let off_in_sector = (byte_off % 512) as usize;

                let sector = self.device.read_sectors(fat_sector, 1, Vec::new())?;

                let raw = u16::from_le_bytes(
                    sector[off_in_sector..off_in_sector + 2].try_into().unwrap(),
                ) as u32;
                let val = raw & FAT16_MASK;

                if val == 0 || val >= 0xFFF8 {
                    return Ok(None);
                }

                if val == 0xFFF7 {
                    return Err(Error::IoError);
                }

                Ok(Some(val))
            }
            FatVariant::Fat12 => {
                // FAT12 uses 1.5 bytes per entry, requiring special handling
                let byte_off = (cluster_number as u64 * 3) / 2;
                let fat_sector = self.first_fat_lba() + (byte_off / 512);
                let off_in_sector = (byte_off % 512) as usize;

                let sector = self.device.read_sectors(fat_sector, 1, Vec::new())?;

                let val = if (cluster_number & 1) == 0 {
                    // Even cluster: use lower 12 bits of the 16-bit value
                    let raw = u16::from_le_bytes(
                        sector[off_in_sector..off_in_sector + 2].try_into().unwrap(),
                    );
                    (raw & 0x0FFF) as u32
                } else {
                    // Odd cluster: use upper 12 bits of the 16-bit value
                    let raw = u16::from_le_bytes(
                        sector[off_in_sector..off_in_sector + 2].try_into().unwrap(),
                    );
                    ((raw >> 4) & 0x0FFF) as u32
                };

                if val == 0 || val >= 0xFF8 {
                    return Ok(None);
                }

                if val == 0xFF7 {
                    return Err(Error::IoError);
                }

                Ok(Some(val))
            }
        }
    }

    /// Resolve a directory entry location (cluster, byte_offset) as returned by `find_dir_entry`
    /// into (base_lba, sector_count) suitable for reading/writing.
    /// For FAT12/16 root dir entries (cluster==0), returns the root dir region.
    /// For cluster-based dirs, returns the cluster's LBA and sectors_per_cluster.
    pub fn dir_entry_region(&self, entry_cluster: u32) -> (u64, u16) {
        if entry_cluster == 0 && matches!(self.variant, FatVariant::Fat12 | FatVariant::Fat16) {
            let root_dir_lba = self.root_dir_lba();
            let root_dir_sectors =
                (self.boot_info.root_entry_count as u64 * 32).div_ceil(512) as u16;
            (root_dir_lba, root_dir_sectors)
        } else {
            let spc = self.boot_info.sectors_per_cluster as u16;
            (self.cluster_to_lba(entry_cluster), spc)
        }
    }

    /// Convert a cluster number (>=2) into an absolute LBA.
    #[inline(always)]
    pub fn cluster_to_lba(&self, cluster: u32) -> u64 {
        // Validate cluster number
        if cluster < 2 {
            return self.partition.starting_lba; // Fallback to partition start
        }

        let reserved = self.boot_info.reserved_sector_count as u64;
        let fatsz = match self.variant {
            FatVariant::Fat32 => self.boot_info.fat_size_32 as u64,
            FatVariant::Fat12 | FatVariant::Fat16 => self.boot_info.fat_size_16 as u64,
        };
        let numfats = self.boot_info.num_fats as u64;
        let root_dir_sectors = match self.variant {
            FatVariant::Fat32 => 0, // FAT32 has cluster-based root
            FatVariant::Fat12 | FatVariant::Fat16 => {
                (self.boot_info.root_entry_count as u64 * 32).div_ceil(512)
            }
        };
        let spc = self.boot_info.sectors_per_cluster as u64;
        let start = self.partition.starting_lba;

        // First sector of data region relative to partition start
        let first_data_sector = reserved + (numfats * fatsz) + root_dir_sectors;

        // Absolute LBA
        let lba = start + ((cluster as u64 - 2) * spc) + first_data_sector;

        // Bounds check to prevent invalid disk access
        if lba >= start + self.partition.size_sectors {
            return start; // Fallback to partition start
        }

        lba
    }

    #[inline(always)]
    pub fn first_fat_lba(&self) -> u64 {
        self.partition.starting_lba + self.boot_info.reserved_sector_count as u64
    }

    #[inline(always)]
    pub fn backup_fat_lba(&self) -> u64 {
        let fat_size = match self.variant {
            FatVariant::Fat32 => self.boot_info.fat_size_32 as u64,
            FatVariant::Fat12 | FatVariant::Fat16 => self.boot_info.fat_size_16 as u64,
        };
        self.partition.starting_lba + self.boot_info.reserved_sector_count as u64 + fat_size
    }

    /// Get root directory entries for FAT12/16
    pub fn get_root_dir_entries(&self) -> Result<Vec<DirectoryRecord>, Error> {
        match self.variant {
            FatVariant::Fat32 => {
                // Should not be called for FAT32
                Err(Error::IoError)
            }
            FatVariant::Fat12 | FatVariant::Fat16 => {
                let root_dir_lba = self.root_dir_lba();
                let root_dir_sectors = (self.boot_info.root_entry_count as u64 * 32).div_ceil(512);

                // Defensive validation to prevent invalid disk access
                if root_dir_sectors == 0 {
                    return Err(Error::Corrupted);
                }
                if root_dir_lba < self.partition.starting_lba
                    || root_dir_lba >= self.partition.starting_lba + self.partition.size_sectors
                {
                    return Err(Error::Corrupted);
                }

                let mut entries = Vec::new();
                let mut buffer = Vec::new();
                let mut lfn_stack: Vec<LongFilenameEntry> = Vec::new();

                for sector in 0..root_dir_sectors {
                    buffer.clear();
                    buffer = self.device.read_sectors(root_dir_lba + sector, 1, buffer)?;

                    let mut offset = 0;
                    while offset + 32 <= buffer.len() {
                        let slice = &buffer[offset..offset + 32];
                        let first = slice[0];
                        if first == 0x00 {
                            return Ok(entries);
                        }
                        if first == 0xE5 {
                            lfn_stack.clear();
                            offset += 32;
                            continue;
                        }

                        let attr = slice[11];
                        if attr == ATTR_LONG_NAME {
                            let lfn: LongFilenameEntry = *bytemuck::from_bytes(slice);
                            if lfn.order & 0x40 != 0 {
                                lfn_stack.clear();
                            }
                            lfn_stack.push(lfn);
                            offset += 32;
                            continue;
                        }

                        let entry: DirectoryEntry = *bytemuck::from_bytes(slice);
                        if entry.is_volume_label() {
                            lfn_stack.clear();
                            offset += 32;
                            continue;
                        }

                        let long_name = if !lfn_stack.is_empty()
                            && lfn_stack.iter().all(|lfn| lfn.attributes == ATTR_LONG_NAME)
                            && lfn_stack.first().is_some_and(|lfn| {
                                let count = (lfn.order & 0x1F) as usize;
                                count == lfn_stack.len()
                            })
                            && lfn_stack
                                .last()
                                .is_some_and(|lfn| lfn.checksum == entry.short_name_checksum())
                        {
                            let name = decode_long_name(&lfn_stack);
                            lfn_stack.clear();
                            name
                        } else {
                            lfn_stack.clear();
                            None
                        };

                        let short_name = entry.fat_name_to_string();
                        entries.push(DirectoryRecord {
                            entry,
                            long_name,
                            short_name,
                        });

                        offset += 32;
                    }
                }

                Ok(entries)
            }
        }
    }

    /// Get root directory LBA for FAT12/16
    #[inline(always)]
    pub fn root_dir_lba(&self) -> u64 {
        let reserved = self.boot_info.reserved_sector_count as u64;
        let fatsz = self.boot_info.fat_size_16 as u64;
        let numfats = self.boot_info.num_fats as u64;

        self.partition.starting_lba + reserved + (numfats * fatsz)
    }

    #[expect(unused)]
    #[inline(always)]
    pub fn first_data_lba(&self) -> u64 {
        let reserved = self.boot_info.reserved_sector_count as u64;
        let fat_size = match self.variant {
            FatVariant::Fat32 => self.boot_info.fat_size_32 as u64,
            FatVariant::Fat12 | FatVariant::Fat16 => self.boot_info.fat_size_16 as u64,
        };
        let numfats = self.boot_info.num_fats as u64;
        let root_dir_sectors = match self.variant {
            FatVariant::Fat32 => 0, // FAT32 has cluster-based root
            FatVariant::Fat12 | FatVariant::Fat16 => {
                (self.boot_info.root_entry_count as u64 * 32).div_ceil(512)
            }
        };

        self.partition.starting_lba + reserved + (numfats * fat_size) + root_dir_sectors
    }

    /// Analyze a cluster chain to find consecutive cluster ranges for batched reading.
    /// Returns a vector of (start_cluster, cluster_count) tuples representing consecutive ranges.
    pub fn analyze_cluster_chain(&self, start_cluster: u32) -> Result<Vec<(u32, u32)>, Error> {
        if start_cluster < 2 {
            return Ok(Vec::new());
        }

        let mut ranges = Vec::new();
        let mut current_cluster = start_cluster;

        loop {
            let range_start = current_cluster;
            let mut range_count = 1u32;

            // Extend the current range as long as clusters are consecutive
            loop {
                match self.get_fat_entry(current_cluster)? {
                    Some(next_cluster) => {
                        // Check if next cluster is consecutive
                        if next_cluster == current_cluster + 1 {
                            current_cluster = next_cluster;
                            range_count += 1;
                        } else {
                            // Non-consecutive cluster found, end current range
                            ranges.push((range_start, range_count));
                            current_cluster = next_cluster;
                            break;
                        }
                    }
                    None => {
                        // End of chain
                        ranges.push((range_start, range_count));
                        return Ok(ranges);
                    }
                }
            }
        }
    }
}
