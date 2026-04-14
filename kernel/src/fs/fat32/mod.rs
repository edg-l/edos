use alloc::{collections::BTreeMap, vec::Vec};
use bytemuck::{Zeroable, cast_ref};

use crate::{
    drivers::ahci::AhciError,
    fs::{
        Error, File, FileSystem, FileTime,
        block_device::BlockDevice,
        fat32::structures::{ATTR_LONG_NAME, DirectoryEntry, Fat32BootSector, FatVariant, FsInfo},
        gpt::Partition,
    },
    log,
    thread::mutex::BlockingMutex,
};

use super::path::Path;

pub mod page_cache;
pub mod read;
pub mod structures;
pub mod traverse;
pub mod write;

/// Side-table entry for a FAT32 inode.
///
/// Keyed by `fat_ino_from_pos(dir_cluster, entry_offset)` in `Fatfs.inode_table`.
/// Populated on first lookup/create; updated on write/truncate; removed only
/// via `evict_inode` (triggered by the final `Arc<VfsInode>` drop). Orphan
/// entries live here until that final drop frees the chain.
#[derive(Debug, Clone)]
pub struct FatInodeEntry {
    /// Cluster that contains this file's directory entry.
    pub dir_cluster: u32,
    /// Byte offset of the short DirectoryEntry within that cluster's data.
    pub entry_offset: u32,
    /// Head cluster of the file's data chain (0 = file has no data yet).
    pub first_cluster: u32,
    /// Current on-disk file size in bytes.
    pub file_size: u32,
    /// True once `remove_file` has detached the dirent. The cluster chain
    /// stays live (to service in-flight reads/writes through existing Arc
    /// refs) until `evict_inode` is called from `VfsInode::drop`.
    pub orphan: bool,
}

/// Encode a (dir_cluster, entry_offset) pair as a FAT32 inode number.
///
/// The encoding is stable as long as the directory entry does not move within
/// its cluster (short-name rewrites preserve position; only LFN slot insertions
/// in v2+ would invalidate it, and those are out of scope for v1).
#[inline]
pub fn fat_ino_from_pos(dir_cluster: u32, entry_offset: usize) -> u64 {
    ((dir_cluster as u64) << 32) | (entry_offset as u64)
}

/// Decode a FAT32 inode number back into (dir_cluster, entry_offset).
#[inline]
#[allow(dead_code)]
pub fn split_fat_ino(ino: u64) -> (u32, u32) {
    let dir_cluster = (ino >> 32) as u32;
    let entry_offset = (ino & 0xFFFF_FFFF) as u32;
    (dir_cluster, entry_offset)
}

#[derive(Debug)]
pub struct Fatfs {
    pub boot_info: Fat32BootSector,
    pub variant: FatVariant,
    pub partition: Partition,
    pub device: BlockDevice,
    /// Protects all FAT write operations (alloc/free/set) against concurrent access.
    pub(super) write_lock: BlockingMutex<()>,
    /// Cached FSInfo sector, protected by a separate mutex so `statfs` can read it
    /// without holding the write lock.
    pub(super) fs_info: BlockingMutex<Option<FsInfo>>,
    /// Per-inode side table for FAT32. Maps ino (dirent-position encoding) to
    /// FatInodeEntry. Only populated for FatVariant::Fat32. FAT12/16 never
    /// write to this table.
    pub(super) inode_table: BlockingMutex<BTreeMap<u64, FatInodeEntry>>,
}

impl Fatfs {
    pub fn new(partition: Partition) -> Result<Self, Error> {
        let device = BlockDevice::new(partition.device_id);
        // Read the boot sector through the page cache (single-sector read from page 0 of partition).
        // Note: the Fatfs helpers are on self, so we call the cache directly here during init.
        let boot_bytes = {
            use crate::fs::block_page_cache::BlockPageCache;
            const SECTORS_PER_PAGE: u64 = 8;
            let page_idx = partition.starting_lba / SECTORS_PER_PAGE;
            let off_in_page = ((partition.starting_lba % SECTORS_PER_PAGE) as usize) * 512;
            let guard = BlockPageCache::global()
                .read_page(partition.device_id, page_idx)
                .map_err(|_| Error::MissingCriticalSectors)?;
            guard.as_slice()[off_in_page..off_in_page + 512].to_vec()
        };

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

                let fs_info_lba = partition.starting_lba + boot_info.fs_info as u64;
                let fs_info_bytes = {
                    use crate::fs::block_page_cache::BlockPageCache;
                    const SECTORS_PER_PAGE: u64 = 8;
                    let page_idx = fs_info_lba / SECTORS_PER_PAGE;
                    let off_in_page = ((fs_info_lba % SECTORS_PER_PAGE) as usize) * 512;
                    let guard = BlockPageCache::global()
                        .read_page(partition.device_id, page_idx)
                        .map_err(|_| Error::MissingCriticalSectors)?;
                    guard.as_slice()[off_in_page..off_in_page + 512].to_vec()
                };

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
            variant,
            device,
            partition,
            write_lock: BlockingMutex::new(()),
            fs_info: BlockingMutex::new(fs_info),
            inode_table: BlockingMutex::new(BTreeMap::new()),
        })
    }
}

// ---- Page-cache I/O helpers --------------------------------------------------
//
// FAT32 does not have a per-file inode page cache (unlike EFS).  All I/O —
// both metadata (FAT table, directory entries) and file data (cluster reads
// and writes) — routes through the block page cache here.  This means cluster
// data is cached at the block level, which is intentional: the cache improves
// repeated reads of the same cluster without any EFS-style per-inode overhead.
impl Fatfs {
    /// Read `sectors` starting at `lba`, returning the data as a Vec<u8>.
    /// Routes through the block page cache (fills from disk on miss).
    pub(super) fn read_disk_sectors(&self, lba: u64, sectors: u16) -> Result<Vec<u8>, Error> {
        const SECTOR_SIZE: usize = 512;
        const SECTORS_PER_PAGE: u64 = 8;
        let total_bytes = sectors as usize * SECTOR_SIZE;
        let mut buffer = alloc::vec![0u8; total_bytes];

        let first_page = lba / SECTORS_PER_PAGE;
        let last_lba = lba + sectors as u64 - 1;
        let last_page = last_lba / SECTORS_PER_PAGE;

        let mut buf_pos = 0usize;
        for page_idx in first_page..=last_page {
            let guard = self.device.read_page(page_idx).map_err(ahci_to_fs)?;
            let page_start_lba = page_idx * SECTORS_PER_PAGE;
            let sec_start = lba.max(page_start_lba) - page_start_lba;
            let sec_end =
                (lba + sectors as u64).min(page_start_lba + SECTORS_PER_PAGE) - page_start_lba;
            let byte_start = sec_start as usize * SECTOR_SIZE;
            let byte_end = sec_end as usize * SECTOR_SIZE;
            let slice = &guard.as_slice()[byte_start..byte_end];
            let len = slice.len();
            buffer[buf_pos..buf_pos + len].copy_from_slice(slice);
            buf_pos += len;
        }
        Ok(buffer)
    }

    /// Write `sectors` starting at `lba`. Routes through the block page cache.
    ///
    /// Splits the range into page-aligned full-page writes (fast path, no RMW)
    /// and partial-page ranges (RMW via write_partial_sectors). The partial
    /// path reads the existing page, patches the sub-range, writes the whole
    /// page back — wasted read if the caller actually wrote a full page.
    pub(super) fn write_disk_sectors(
        &self,
        lba: u64,
        data: &[u8],
        sectors: u16,
    ) -> Result<(), Error> {
        const SECTOR_SIZE: usize = 512;
        const SECTORS_PER_PAGE: u64 = 8;
        const PAGE_SIZE: usize = 4096;

        debug_assert_eq!(data.len(), sectors as usize * SECTOR_SIZE);

        let mut cur_lba = lba;
        let mut remaining = sectors as u64;
        let mut data_pos = 0usize;

        while remaining > 0 {
            let sector_in_page = cur_lba & (SECTORS_PER_PAGE - 1);
            let page_idx = cur_lba / SECTORS_PER_PAGE;

            if sector_in_page == 0 && remaining >= SECTORS_PER_PAGE {
                // Aligned full-page write: no read-modify-write.
                let page: &[u8; PAGE_SIZE] = data[data_pos..data_pos + PAGE_SIZE]
                    .try_into()
                    .expect("exact 4 KiB slice");
                self.device.write_page(page_idx, page).map_err(ahci_to_fs)?;
                cur_lba += SECTORS_PER_PAGE;
                remaining -= SECTORS_PER_PAGE;
                data_pos += PAGE_SIZE;
            } else {
                // Partial range within one page: RMW.
                let take = (SECTORS_PER_PAGE - sector_in_page).min(remaining);
                let bytes = take as usize * SECTOR_SIZE;
                self.device
                    .write_partial_sectors(cur_lba, take as u16, &data[data_pos..data_pos + bytes])
                    .map_err(ahci_to_fs)?;
                cur_lba += take;
                remaining -= take;
                data_pos += bytes;
            }
        }
        Ok(())
    }
}

fn ahci_to_fs(e: AhciError) -> Error {
    match e {
        AhciError::IoError => Error::IoError,
        _ => Error::IoError,
    }
}

impl FileSystem for Fatfs {
    fn list_files(&self, path: &Path) -> Result<alloc::vec::Vec<super::File>, super::Error> {
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
        &self,
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

    fn write_bytes(&self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, super::Error> {
        if data.is_empty() {
            return Ok(0);
        }
        let _guard = self.write_lock.lock();

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

        // Update the side table for FAT32 (the write may have allocated a head cluster).
        if matches!(self.variant, FatVariant::Fat32) {
            debug_assert!(eo <= u32::MAX as usize, "entry_offset overflows u32");
            let ino = fat_ino_from_pos(ec, eo);
            let mut table = self.inode_table.lock();
            let e = table.entry(ino).or_insert_with(|| FatInodeEntry {
                dir_cluster: ec,
                entry_offset: eo as u32,
                first_cluster: entry.first_cluster(),
                file_size: new_size as u32,
                orphan: false,
            });
            e.first_cluster = entry.first_cluster();
            e.file_size = new_size as u32;
        }

        Ok(written)
    }

    fn create_file(&self, path: &Path) -> Result<(), super::Error> {
        use crate::fs::fat32::structures::DirectoryEntry;
        use bytemuck::Zeroable;

        let _guard = self.write_lock.lock();

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
        let (dirent_cluster, dirent_offset) =
            self.append_dir_entry(parent_cluster, &de, long_name)?;

        // Populate the side table for FAT32.
        if matches!(self.variant, FatVariant::Fat32) {
            debug_assert!(
                dirent_offset <= u32::MAX as usize,
                "entry_offset overflows u32"
            );
            let ino = fat_ino_from_pos(dirent_cluster, dirent_offset);
            self.inode_table.lock().insert(
                ino,
                FatInodeEntry {
                    dir_cluster: dirent_cluster,
                    entry_offset: dirent_offset as u32,
                    first_cluster: 0,
                    file_size: 0,
                    orphan: false,
                },
            );
        }

        Ok(())
    }

    fn create_dir(&self, path: &Path) -> Result<(), super::Error> {
        let _guard = self.write_lock.lock();

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

        self.write_disk_sectors(self.cluster_to_lba(newc), &dirbuf, spc)?;

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

    fn remove_file(&self, path: &Path) -> Result<(), super::Error> {
        let _guard = self.write_lock.lock();
        let (parent_cluster, _) = self.resolve_parent_and_name(path)?;

        // Locate the entry and its position in parent directory
        let (entry, dir_cluster, entry_off) = match self.find_dir_entry(path)? {
            Some((e, c, o)) => (e, c, o),
            None => return Err(Error::FileNotFound),
        };
        if entry.is_directory() {
            return Err(Error::NotAFile);
        }

        // Linux-style orphan-inode removal:
        //   FAT32: mark the dirent 0xE5 (so new opens fail ENOENT) and flip
        //     the side-table entry's `orphan` flag. The cluster chain stays
        //     live and reachable by ino so existing fds/VMAs keep working;
        //     `evict_inode` frees the chain when the final Arc<VfsInode>
        //     drops.
        //   FAT12/16: PageCacheOps is not exposed for these variants, so no
        //     mmap path exists and we free the chain inline. Open fds across
        //     unlink is not supported on FAT12/16 here.
        let is_fat32 = matches!(self.variant, FatVariant::Fat32);

        if !is_fat32 {
            let start = entry.first_cluster();
            if start >= 2 {
                let _freed = self.free_cluster_chain(start)?;
            }
        }

        // Mark the directory entry deleted (0xE5).
        let (base_lba, sectors) = self.dir_entry_region(dir_cluster);
        let mut buf = self.read_disk_sectors(base_lba, sectors)?;
        if entry_off + 32 > buf.len() {
            return Err(Error::IoError);
        }
        buf[entry_off] = 0xE5;
        self.write_disk_sectors(base_lba, &buf, sectors)?;

        self.delete_long_name_sequence(
            parent_cluster,
            dir_cluster,
            entry_off,
            entry.short_name_checksum(),
        )?;

        if is_fat32 {
            let ino = fat_ino_from_pos(dir_cluster, entry_off);
            let mut table = self.inode_table.lock();
            if let Some(e) = table.get_mut(&ino) {
                e.orphan = true;
            }
        }

        Ok(())
    }

    fn remove_dir(&self, path: &Path) -> Result<(), super::Error> {
        let _guard = self.write_lock.lock();

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

        let mut read_buffer;
        // Silence: read_buffer is always assigned before use below.
        // Ensure the directory is empty (only "." and ".." allowed)
        let mut cur = entry.first_cluster();
        if cur >= 2 {
            let spc = self.boot_info.sectors_per_cluster as u16;
            loop {
                let base_lba = self.cluster_to_lba(cur);
                read_buffer = self.read_disk_sectors(base_lba, spc)?;
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

        read_buffer = self.read_disk_sectors(base_lba, sectors)?;
        if entry_off + 32 > read_buffer.len() {
            return Err(Error::IoError);
        }
        read_buffer[entry_off] = 0xE5;
        self.write_disk_sectors(base_lba, &read_buffer, sectors)?;

        self.delete_long_name_sequence(
            parent_cluster,
            dir_cluster,
            entry_off,
            entry.short_name_checksum(),
        )?;

        Ok(())
    }

    fn file_info(&self, path: &Path) -> Result<super::File, super::Error> {
        if let Some((entry, _, _)) = self.find_dir_entry(path)? {
            Ok(entry.into())
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn flush(&self) -> Result<(), Error> {
        let _guard = self.write_lock.lock();
        // Only save FSInfo for FAT32
        if matches!(self.variant, FatVariant::Fat32) {
            self.save_fs_info()?;
        }
        self.device.flush()?;

        Ok(())
    }

    fn flush_inode(&self, _ino: u64) -> Result<(), Error> {
        // Flush drive write cache only; metadata is already on disk via
        // BlockPageCache writeback, and file data went through direct AHCI.
        self.device.flush()?;
        Ok(())
    }

    fn as_page_cache_ops(&self) -> Option<&dyn crate::fs::page_cache::PageCacheOps> {
        self.as_page_cache_ops_fat32()
    }

    fn file_size_ino(&self, ino: u64) -> Result<u64, Error> {
        self.file_size_ino_fat32(ino)
    }

    fn evict_inode(&self, ino: u64) -> Result<(), Error> {
        self.evict_inode_fat32(ino)
    }

    fn truncate(&self, path: &Path, size: u64) -> Result<(), Error> {
        let _guard = self.write_lock.lock();
        let (entry, ec, eo) = match self.find_dir_entry(path)? {
            Some((e, c, o)) if !e.is_directory() => (e, c, o),
            Some(_) => return Err(Error::NotAFile),
            None => return Err(Error::FileNotFound),
        };

        if size > u32::MAX as u64 {
            return Err(Error::IoError);
        }

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

            // Update side table for FAT32.
            if matches!(self.variant, FatVariant::Fat32) {
                debug_assert!(eo <= u32::MAX as usize, "entry_offset overflows u32");
                let ino = fat_ino_from_pos(ec, eo);
                if let Some(e) = self.inode_table.lock().get_mut(&ino) {
                    e.first_cluster = 0;
                    e.file_size = 0;
                }
            }
        } else {
            // For non-zero truncate, update the directory entry size.
            // Clusters beyond the new size are left allocated but inaccessible.
            self.patch_dir_entry_at(ec, eo, |de| {
                de.file_size = size as u32;
            })?;

            // Update side table for FAT32 (first_cluster unchanged for size > 0).
            if matches!(self.variant, FatVariant::Fat32) {
                debug_assert!(eo <= u32::MAX as usize, "entry_offset overflows u32");
                let ino = fat_ino_from_pos(ec, eo);
                if let Some(e) = self.inode_table.lock().get_mut(&ino) {
                    e.file_size = size as u32;
                }
            }
        }

        Ok(())
    }

    fn rename(&self, old_path: &Path, new_path: &Path) -> Result<(), Error> {
        let _guard = self.write_lock.lock();
        let old_path = old_path.normalize();
        let new_path = new_path.normalize();
        // Inode stability invariant: v1 only supports same-directory renames with
        // short-name rewrites. The directory entry stays in the same cluster at the
        // same byte offset, so (dir_cluster, entry_offset) -- and thus the ino -- is
        // unchanged. The side table entry is left alone here.

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

    fn resolve_inode(&self, path: &Path) -> Result<u64, Error> {
        if path.normalize().is_root() {
            return Ok(self.boot_info.root_cluster as u64);
        }

        // FAT12/16: use legacy first-cluster ino; no side table.
        if !matches!(self.variant, FatVariant::Fat32) {
            return if let Some((entry, _, _)) = self.find_dir_entry(path)? {
                Ok(entry.first_cluster() as u64)
            } else {
                Err(Error::FileNotFound)
            };
        }

        // FAT32: use dirent-position encoding and maintain the side table.
        let (entry, dc, eo) = match self.find_dir_entry(path)? {
            Some(x) => x,
            None => return Err(Error::FileNotFound),
        };

        debug_assert!(eo <= u32::MAX as usize, "entry_offset overflows u32");
        let ino = fat_ino_from_pos(dc, eo);

        {
            let mut table = self.inode_table.lock();
            let e = table.entry(ino).or_insert_with(|| FatInodeEntry {
                dir_cluster: dc,
                entry_offset: eo as u32,
                first_cluster: entry.first_cluster(),
                file_size: entry.file_size,
                orphan: false,
            });
            // Refresh data-cluster and size; preserve orphan flag.
            e.first_cluster = entry.first_cluster();
            e.file_size = entry.file_size;
        }

        Ok(ino)
    }

    fn statfs(&self) -> Result<super::StatFs, Error> {
        let bs = &self.boot_info;
        let cluster_size = bs.bytes_per_sector as u64 * bs.sectors_per_cluster as u64;
        let total_clusters = bs.calculate_cluster_count() as u64;
        let total_size = total_clusters * cluster_size;
        let total_blocks = total_size / cluster_size;

        let free_clusters = match &*self.fs_info.lock() {
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
            version: 0,
            block_groups: 0,
        })
    }
}
