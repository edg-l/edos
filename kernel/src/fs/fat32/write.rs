use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use bytemuck::{bytes_of, from_bytes};

use crate::{
    fs::{
        Error,
        fat32::{
            Fatfs, sector_span,
            structures::{
                ATTR_LONG_NAME, CLUSTER_EOF, CLUSTER_FREE, DirectoryEntry, DirectoryRecord,
                FAT16_MASK, FAT32_MASK, FatVariant, LongFilenameEntry,
            },
        },
        path::Path,
    },
    log,
};

impl Fatfs {
    /// Overwrite file contents starting at offset 0.
    /// Extends the cluster chain if `buf` is larger than the current file.
    /// Does not shrink or update the on-disk directory entry size yet.
    #[expect(unused)]
    pub fn write_file(
        &self,
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
        &self,
        entry: &mut DirectoryEntry,
        entry_pos: Option<(u32, usize)>, // needed only if entry.first_cluster()<2
        offset: u64,
        buf: &[u8],
    ) -> Result<usize, Error> {
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
            let (dir_cluster, entry_off) = entry_pos.ok_or(Error::InvalidArgument)?;
            head = self.alloc_cluster()?;
            let mut last = head;
            for _ in 0..target_idx {
                let nc = self.alloc_cluster()?;
                self.link_fat_entry(last, nc)?;
                last = nc;
            }

            // Patch on-disk entry with new head
            let (base_lba, region_sectors) = self.dir_entry_region(dir_cluster);
            let mut dirbuf = self.read_disk_sectors(base_lba, region_sectors)?;
            if entry_off + 32 > dirbuf.len() {
                return Err(Error::Corrupted);
            }
            let mut de: DirectoryEntry = *bytemuck::from_bytes(&dirbuf[entry_off..entry_off + 32]);
            de.first_cluster_high = ((head >> 16) & 0xFFFF) as u16;
            de.first_cluster_low = (head & 0xFFFF) as u16;
            let bytes: [u8; 32] = bytemuck::cast(de);
            dirbuf[entry_off..entry_off + 32].copy_from_slice(&bytes);
            self.write_disk_sectors(base_lba, &dirbuf, region_sectors)?;

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
            cluster = self.get_fat_entry(cluster)?.ok_or(Error::Corrupted)?;
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
                let v = self.read_disk_sectors(lba, spc_u16)?;
                if v.len() != bpc {
                    return Err(Error::Corrupted);
                }
                v
            };

            let space = bpc - inner_off;
            let take = space.min(remaining);
            scratch[inner_off..inner_off + take].copy_from_slice(&buf[buf_off..buf_off + take]);

            self.write_disk_sectors(lba, &scratch, spc_u16)?;

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
    pub fn alloc_cluster(&self) -> Result<u32, Error> {
        let bytes_per_sector = self.boot_info.bytes_per_sector as usize;
        let fat_sectors = match self.variant {
            FatVariant::Fat32 => self.boot_info.fat_size_32 as u64,
            FatVariant::Fat12 | FatVariant::Fat16 => self.boot_info.fat_size_16 as u64,
        };
        let entries_per_sector = match self.variant {
            FatVariant::Fat32 => bytes_per_sector / 4,
            FatVariant::Fat16 => bytes_per_sector / 2,
            FatVariant::Fat12 => bytes_per_sector * 2 / 3,
        };

        // Start hint from FSInfo if valid (FAT32 only), else 2
        let mut start_cluster = match self.variant {
            FatVariant::Fat32 => {
                let fs_info = self.fs_info.lock();
                if let Some(ref fi) = *fs_info {
                    if fi.next_free != 0xFFFF_FFFF {
                        fi.next_free
                    } else {
                        2
                    }
                } else {
                    2
                }
            }
            FatVariant::Fat12 | FatVariant::Fat16 => 2,
        };
        if start_cluster < 2 {
            start_cluster = 2;
        }

        // Search for a free cluster starting at `start`, scanning up to max_clusters.
        // Returns the allocated cluster number on success.
        let variant = self.variant;
        let search = |start: u32| -> Option<u32> {
            let mut current_cluster = start;
            let max_clusters = entries_per_sector as u64 * fat_sectors;
            let mut sec;
            while (current_cluster as u64) < max_clusters {
                let (byte_index, entry_size) = match variant {
                    FatVariant::Fat32 => ((current_cluster as usize) * 4, 4usize),
                    FatVariant::Fat16 => ((current_cluster as usize) * 2, 2usize),
                    FatVariant::Fat12 => ((current_cluster as usize * 3) / 2, 2usize),
                };
                let sector_index = (byte_index / bytes_per_sector) as u64;
                let within = byte_index % bytes_per_sector;

                let span = sector_span(within, entry_size, bytes_per_sector);
                sec = self
                    .read_disk_sectors(self.first_fat_lba() + sector_index, span)
                    .ok()?;

                let val = match variant {
                    FatVariant::Fat32 => {
                        u32::from_le_bytes(sec[within..within + 4].try_into().ok()?) & FAT32_MASK
                    }
                    FatVariant::Fat16 => {
                        u16::from_le_bytes(sec[within..within + 2].try_into().ok()?) as u32
                            & FAT16_MASK
                    }
                    FatVariant::Fat12 => {
                        let raw = u16::from_le_bytes(sec[within..within + 2].try_into().ok()?);
                        if current_cluster & 1 == 0 {
                            (raw & 0x0FFF) as u32
                        } else {
                            ((raw >> 4) & 0x0FFF) as u32
                        }
                    }
                };

                if val == crate::fs::fat32::structures::CLUSTER_FREE {
                    // Mark as EOF using variant-aware write
                    match variant {
                        FatVariant::Fat32 => {
                            let eof = crate::fs::fat32::structures::CLUSTER_EOF;
                            sec[within..within + 4].copy_from_slice(&eof.to_le_bytes());
                        }
                        FatVariant::Fat16 => {
                            let eof: u16 = 0xFFFF;
                            sec[within..within + 2].copy_from_slice(&eof.to_le_bytes());
                        }
                        FatVariant::Fat12 => {
                            let existing =
                                u16::from_le_bytes(sec[within..within + 2].try_into().ok()?);
                            let new_val = if current_cluster & 1 == 0 {
                                (existing & 0xF000) | 0x0FFF
                            } else {
                                (existing & 0x000F) | (0xFFF << 4)
                            };
                            sec[within..within + 2].copy_from_slice(&new_val.to_le_bytes());
                        }
                    }

                    self.write_fat_sectors(sector_index, &sec.clone(), span)
                        .ok()?;

                    return Some(current_cluster);
                }
                current_cluster += 1;
            }
            None
        };

        // First pass from hint, then second pass from 2
        let found = search(start_cluster).or_else(|| search(2));

        match found {
            Some(c) => {
                // Update FSInfo in memory (FAT32 only)
                let next = if c == u32::MAX {
                    2
                } else {
                    c.saturating_add(1).max(2)
                };
                let mut fs_info = self.fs_info.lock();
                if let Some(ref mut fi) = *fs_info {
                    fi.next_free = next;
                    if fi.free_count != 0xFFFF_FFFF && fi.free_count > 0 {
                        fi.free_count -= 1;
                    }
                }
                Ok(c)
            }
            None => Err(Error::NoSpace),
        }
    }

    /// Link `from` cluster to `to` cluster by setting FAT[from] = to,
    /// and set FAT[to] to EOF.
    pub fn link_fat_entry(&self, from: u32, to: u32) -> Result<(), Error> {
        self.set_fat_value(from, to)?;
        let eof_val = match self.variant {
            FatVariant::Fat32 => CLUSTER_EOF,
            FatVariant::Fat16 => 0xFFFF,
            FatVariant::Fat12 => 0xFFF,
        };
        self.set_fat_value(to, eof_val)?;
        Ok(())
    }

    /// Write `sectors` sectors of FAT content at `sector_index` into every FAT
    /// the volume carries, so the mirrors do not diverge from the primary.
    pub(super) fn write_fat_sectors(
        &self,
        sector_index: u64,
        buf: &[u8],
        sectors: u16,
    ) -> Result<(), Error> {
        self.write_disk_sectors(self.first_fat_lba() + sector_index, buf, sectors)?;
        if self.boot_info.num_fats > 1 {
            self.write_disk_sectors(self.backup_fat_lba() + sector_index, buf, sectors)?;
        }
        Ok(())
    }

    /// Low-level setter for a FAT entry. Writes to every FAT.
    fn set_fat_value(&self, cluster: u32, value: u32) -> Result<(), Error> {
        let bytes_per_sector = self.boot_info.bytes_per_sector as usize;

        match self.variant {
            FatVariant::Fat32 => {
                let byte_index = (cluster as usize) * 4;
                let sector_index = (byte_index / bytes_per_sector) as u64;
                let within = byte_index % bytes_per_sector;

                let mut sec = self.read_disk_sectors(self.first_fat_lba() + sector_index, 1)?;
                let v = value & FAT32_MASK;
                sec[within..within + 4].copy_from_slice(&v.to_le_bytes());
                self.write_fat_sectors(sector_index, &sec, 1)?;
            }
            FatVariant::Fat16 => {
                let byte_index = (cluster as usize) * 2;
                let sector_index = (byte_index / bytes_per_sector) as u64;
                let within = byte_index % bytes_per_sector;

                let mut sec = self.read_disk_sectors(self.first_fat_lba() + sector_index, 1)?;
                let v = (value & FAT16_MASK) as u16;
                sec[within..within + 2].copy_from_slice(&v.to_le_bytes());
                self.write_fat_sectors(sector_index, &sec, 1)?;
            }
            FatVariant::Fat12 => {
                // FAT12 uses 1.5 bytes per entry, requiring special handling
                let byte_offset = (cluster as u64 * 3) / 2;
                let sector_index = byte_offset / bytes_per_sector as u64;
                let within = (byte_offset % bytes_per_sector as u64) as usize;

                let span = sector_span(within, 2, bytes_per_sector);
                let mut sec = self.read_disk_sectors(self.first_fat_lba() + sector_index, span)?;

                let existing = u16::from_le_bytes([sec[within], sec[within + 1]]);
                let new_val = if (cluster & 1) == 0 {
                    // Even cluster: lower 12 bits
                    (existing & 0xF000) | ((value & 0x0FFF) as u16)
                } else {
                    // Odd cluster: upper 12 bits
                    (existing & 0x000F) | (((value & 0x0FFF) as u16) << 4)
                };
                sec[within..within + 2].copy_from_slice(&new_val.to_le_bytes());

                self.write_fat_sectors(sector_index, &sec, span)?;
            }
        }
        Ok(())
    }

    pub fn save_fs_info(&self) -> Result<(), Error> {
        let fs_info = self.fs_info.lock();
        if let Some(ref fi) = *fs_info {
            let data: Vec<u8> = bytes_of(fi).to_vec(); // 512 bytes
            self.write_disk_sectors(
                self.partition.starting_lba + self.boot_info.fs_info as u64,
                &data,
                1,
            )?;
        }
        Ok(())
    }

    pub fn patch_dir_entry_at(
        &self,
        entry_cluster: u32,
        entry_offset: usize, // byte offset within the region
        patch: impl FnOnce(&mut DirectoryEntry),
    ) -> Result<(), Error> {
        let (base_lba, sectors) = self.dir_entry_region(entry_cluster);

        let mut buf = self.read_disk_sectors(base_lba, sectors)?;
        if entry_offset + 32 > buf.len() {
            return Err(Error::Corrupted);
        }

        let mut de: DirectoryEntry = *bytemuck::from_bytes(&buf[entry_offset..entry_offset + 32]);
        patch(&mut de);
        let bytes: [u8; 32] = bytemuck::cast(de);
        buf[entry_offset..entry_offset + 32].copy_from_slice(&bytes);

        self.write_disk_sectors(base_lba, &buf, sectors)?;
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
        let mut dir_cluster = self.root_dir_cluster();
        for comp in parent_components {
            let entries = if self.is_fixed_root(dir_cluster) {
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
                None => return Err(Error::FileNotFound),
            };
            dir_cluster = de.first_cluster();
        }

        Ok((dir_cluster, name))
    }

    pub(super) fn generate_short_name(
        &self,
        dir_cluster: u32,
        desired: &str,
    ) -> Result<(String, bool), Error> {
        let existing_records = if self.is_fixed_root(dir_cluster) {
            self.get_root_dir_entries()?
        } else {
            self.get_dir_entries(dir_cluster)?
        };

        let existing_short: Vec<[u8; 11]> = existing_records
            .iter()
            .map(DirectoryRecord::short_name_bytes)
            .collect();

        if DirectoryEntry::is_valid_short_name(desired) {
            let short_fat = DirectoryEntry::string_to_fat_name(desired);
            let conflict = existing_short.iter().any(|entry| entry == &short_fat);
            if !conflict {
                return Ok((desired.to_string(), false));
            }
        }

        let (base_part, ext_part) = desired
            .rsplit_once('.')
            .map(|(b, e)| (b, Some(e)))
            .unwrap_or((desired, None));

        let mut base_clean = sanitize_short_component(base_part);
        if base_clean.is_empty() {
            base_clean.push(b'_');
        }

        let ext_clean_full = ext_part.map(sanitize_short_component).unwrap_or_default();
        let ext_len = core::cmp::min(ext_clean_full.len(), 3);
        let ext_slice = &ext_clean_full[..ext_len];
        let ext_str = core::str::from_utf8(ext_slice).unwrap_or("");

        let mut counter = 1usize;
        loop {
            let suffix = format!("~{}", counter);
            let suffix_bytes = suffix.as_bytes();
            let mut base_candidate = base_clean.clone();
            let max_base_len = 8usize.saturating_sub(suffix_bytes.len());
            if base_candidate.len() > max_base_len {
                base_candidate.truncate(max_base_len);
            }
            if base_candidate.is_empty() {
                if suffix_bytes.len() >= 8 {
                    base_candidate.extend_from_slice(&suffix_bytes[suffix_bytes.len() - 8..]);
                } else {
                    base_candidate.extend_from_slice(suffix_bytes);
                }
            } else {
                base_candidate.extend_from_slice(suffix_bytes);
            }
            if base_candidate.len() > 8 {
                base_candidate.truncate(8);
            }
            if base_candidate.is_empty() {
                base_candidate.push(b'_');
            }

            let base_str = core::str::from_utf8(&base_candidate).unwrap_or("_");
            let candidate_string = if ext_len == 0 {
                base_str.to_string()
            } else {
                format!("{}.{}", base_str, ext_str)
            };

            let candidate_fat = DirectoryEntry::string_to_fat_name(&candidate_string);
            if !existing_short.iter().any(|entry| entry == &candidate_fat) {
                return Ok((candidate_string, true));
            }

            counter += 1;
        }
    }

    /// Append a short 8.3 DirectoryEntry to a directory, allocating a new cluster if needed.
    /// Returns (cluster_that_contains_entry, byte_offset_within_cluster).
    pub fn append_dir_entry(
        &self,
        mut dir_cluster: u32,
        new_entry: &DirectoryEntry,
        long_name: Option<&str>,
    ) -> Result<(u32, usize), Error> {
        let spc = self.boot_info.sectors_per_cluster as u16;
        let bps = self.boot_info.bytes_per_sector as usize;
        let cluster_bytes = bps * spc as usize;

        let lfn_entries = long_name
            .map(|name| build_lfn_entries(name, new_entry.short_name_checksum()))
            .unwrap_or_default();
        let needed_slots = lfn_entries.len() + 1;

        let mut buf;
        loop {
            let (base_lba, sectors) = self.dir_entry_region(dir_cluster);
            buf = self.read_disk_sectors(base_lba, sectors)?;
            if buf.len() < bps * sectors as usize {
                return Err(Error::Corrupted);
            }

            // Scan for contiguous free slots
            let mut off = 0usize;
            while off + 32 <= buf.len() {
                let mut run = 0usize;
                while run < needed_slots
                    && off + (run + 1) * 32 <= buf.len()
                    && matches!(buf[off + run * 32], 0x00 | 0xE5)
                {
                    run += 1;
                }

                if run >= needed_slots {
                    let mut pos = off;
                    for entry in &lfn_entries {
                        buf[pos..pos + 32].copy_from_slice(entry);
                        pos += 32;
                    }
                    let bytes: [u8; 32] = bytemuck::cast(*new_entry);
                    buf[pos..pos + 32].copy_from_slice(&bytes);
                    self.write_disk_sectors(base_lba, &buf, sectors)?;
                    return Ok((dir_cluster, pos));
                }

                off += 32;
            }

            // The FAT12/16 root is a fixed region with no chain behind it.
            if self.is_fixed_root(dir_cluster) {
                return Err(Error::NoSpace);
            }

            // No space here. Advance or extend directory chain.
            match self.get_fat_entry(dir_cluster)? {
                Some(next) => dir_cluster = next,
                None => {
                    let newc = self.alloc_cluster()?;
                    self.link_fat_entry(dir_cluster, newc)?;
                    let mut new_buf = alloc::vec![0u8; cluster_bytes];
                    let mut pos = 0usize;
                    for entry in &lfn_entries {
                        new_buf[pos..pos + 32].copy_from_slice(entry);
                        pos += 32;
                    }
                    let bytes: [u8; 32] = bytemuck::cast(*new_entry);
                    new_buf[pos..pos + 32].copy_from_slice(&bytes);
                    self.write_disk_sectors(self.cluster_to_lba(newc), &new_buf, spc)?;
                    return Ok((newc, pos));
                }
            }
        }
    }

    fn mark_entry_deleted(&self, cluster: u32, entry_offset: usize) -> Result<(), Error> {
        let (base_lba, sectors) = self.dir_entry_region(cluster);
        let mut buf = self.read_disk_sectors(base_lba, sectors)?;
        if entry_offset + 32 > buf.len() {
            return Err(Error::Corrupted);
        }
        buf[entry_offset] = 0xE5;
        self.write_disk_sectors(base_lba, &buf, sectors)?;
        Ok(())
    }

    pub(super) fn delete_long_name_sequence(
        &self,
        dir_head: u32,
        target_cluster: u32,
        target_offset: usize,
        checksum: u8,
    ) -> Result<(), Error> {
        let mut cluster = dir_head;
        let mut pending: Vec<(u32, usize, LongFilenameEntry)> = Vec::new();

        loop {
            let (base_lba, sectors) = self.dir_entry_region(cluster);
            let buf = self.read_disk_sectors(base_lba, sectors)?;
            let mut off = 0usize;

            while off + 32 <= buf.len() {
                let slice = &buf[off..off + 32];
                let first = slice[0];
                if first == 0x00 {
                    return Ok(());
                }
                if first == 0xE5 {
                    pending.clear();
                    off += 32;
                    continue;
                }

                let attr = slice[11];
                if attr == ATTR_LONG_NAME {
                    let lfn: LongFilenameEntry = *from_bytes(slice);
                    pending.push((cluster, off, lfn));
                    off += 32;
                    continue;
                }

                if cluster == target_cluster && off == target_offset {
                    for (lfn_cluster, lfn_off, lfn_entry) in pending.iter().rev() {
                        if lfn_entry.checksum == checksum {
                            self.mark_entry_deleted(*lfn_cluster, *lfn_off)?;
                        }
                    }
                    return Ok(());
                }

                pending.clear();
                off += 32;
            }

            if self.is_fixed_root(cluster) {
                return Ok(());
            }

            match self.get_fat_entry(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(()),
            }
        }
    }

    pub(super) fn free_cluster_chain(&self, start: u32) -> Result<u32, Error> {
        if start < 2 {
            return Ok(0);
        }

        // Hard guard to avoid infinite loops on corrupted chains
        let fat_size = match self.variant {
            FatVariant::Fat32 => self.boot_info.fat_size_32 as usize,
            FatVariant::Fat12 | FatVariant::Fat16 => self.boot_info.fat_size_16 as usize,
        };
        let entry_stride = match self.variant {
            FatVariant::Fat32 => 4,
            FatVariant::Fat16 => 2,
            FatVariant::Fat12 => 2, // approximate (1.5 bytes), conservative bound
        };
        let fat_entries = (fat_size * self.boot_info.bytes_per_sector as usize) / entry_stride;

        let mut freed: u32 = 0;
        let mut cur = start;
        let mut visited = 0usize;

        loop {
            visited += 1;
            if visited > fat_entries {
                // cycle or corruption
                log!("Possible corruption");
                return Err(Error::Corrupted);
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

        // Update FSInfo in memory (FAT32 only)
        {
            let mut fs_info = self.fs_info.lock();
            if let Some(ref mut fi) = *fs_info {
                if fi.free_count != 0xFFFF_FFFF {
                    fi.free_count = fi.free_count.saturating_add(freed);
                }
                if fi.next_free == 0xFFFF_FFFF || start < fi.next_free {
                    fi.next_free = start;
                }
            }
        }

        Ok(freed)
    }

    /// Copy the entire primary FAT to the backup FAT.
    #[expect(unused)]
    pub fn mirror_primary_fat_to_backup(&self) -> Result<(), Error> {
        let total: u64 = self.boot_info.fat_size_32 as u64; // sectors in one FAT
        let mut src_lba = self.first_fat_lba();
        let mut dst_lba = self.backup_fat_lba();

        // Copy in chunks to avoid large allocations
        let chunk: u16 = 64; // sectors per transfer
        let mut remaining = total;

        let mut buf = Vec::new();
        while remaining > 0 {
            let take = core::cmp::min(remaining, chunk as u64) as u16;
            buf = self.read_disk_sectors(src_lba, take)?;
            self.write_disk_sectors(dst_lba, &buf, take)?;

            src_lba += take as u64;
            dst_lba += take as u64;
            remaining -= take as u64;
        }

        Ok(())
    }
}

fn sanitize_short_component(component: &str) -> Vec<u8> {
    component
        .chars()
        .map(|ch| {
            let upper = ch.to_ascii_uppercase();
            match upper {
                'A'..='Z' | '0'..='9' => upper as u8,
                '%' | '\'' | '-' | '_' | '@' | '~' | '`' | '!' | '(' | ')' | '{' | '}' | '^'
                | '#' | '&' => upper as u8,
                ' ' => b'_',
                _ => b'_',
            }
        })
        .collect()
}

fn build_lfn_entries(name: &str, checksum: u8) -> Vec<[u8; 32]> {
    let mut units: Vec<u16> = name.encode_utf16().collect();
    units.push(0);
    while !units.len().is_multiple_of(13) {
        units.push(0xFFFF);
    }

    let chunks = units.chunks(13);
    let total = chunks.len();
    let mut entries: Vec<[u8; 32]> = Vec::with_capacity(total);

    for (idx, chunk) in chunks.enumerate() {
        let mut entry = LongFilenameEntry {
            order: (idx + 1) as u8,
            name1: [0u16; 5],
            attributes: ATTR_LONG_NAME,
            entry_type: 0,
            checksum,
            name2: [0u16; 6],
            first_cluster_low: 0,
            name3: [0u16; 2],
        };
        if idx + 1 == total {
            entry.order |= 0x40;
        }

        // Field-at-a-time assignment: the struct is `packed`, so a `&mut` to any
        // of these arrays (what `copy_from_slice` needs) would be unaligned.
        entry.name1 = chunk[..5].try_into().unwrap();
        entry.name2 = chunk[5..11].try_into().unwrap();
        entry.name3 = chunk[11..13].try_into().unwrap();

        entries.push(bytemuck::cast(entry));
    }

    entries.reverse();
    entries
}
