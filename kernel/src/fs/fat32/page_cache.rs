//! FAT32 PageCacheOps implementation.
//!
//! File data I/O routes exclusively through the block-io trait (bypassing
//! `BlockPageCache`) per the project invariant for file data paths.
//!
//! # Cluster chain walking
//! `cluster_at_index` walks from the head on every call, so `fill_page` is O(N)
//! in cluster count; sequential fill over a large file is O(N^2). `flush_page`
//! uses `ensure_chain_to` which walks once per call and returns both the head
//! and the target cluster, saving a second walk per iteration. Future work can
//! memoize the last visited position for both paths.

use alloc::{collections::BTreeSet, vec};

use crate::{
    drivers::block_io::{self, BlockBuffer, WriteFlags},
    fs::{
        Error, FileTime,
        fat32::{FatInodeEntry, Fatfs},
        page_cache::PageCacheOps,
    },
};

const PAGE_SIZE: usize = 4096;
const SECTOR_SIZE: usize = 512;

// ---------------------------------------------------------------------------
// Side-table helper
// ---------------------------------------------------------------------------

impl Fatfs {
    /// Look up a cloned `FatInodeEntry` from the side table.
    /// Returns `Error::FileNotFound` if the ino is not in the table.
    pub(super) fn lookup_inode_entry(&self, ino: u64) -> Result<FatInodeEntry, Error> {
        self.inode_table
            .lock()
            .get(&ino)
            .cloned()
            .ok_or(Error::FileNotFound)
    }

    // ---------------------------------------------------------------------------
    // Cluster-chain helpers
    // ---------------------------------------------------------------------------

    /// Walk the cluster chain starting at `head` and return the cluster at logical
    /// index `idx` (0-based). Returns `None` if the chain ends before reaching `idx`.
    ///
    /// A `BTreeSet<u32>` guards against cycles (corrupted FAT). If a cycle is
    /// detected `Error::Corrupted` is returned.
    pub(super) fn cluster_at_index(&self, head: u32, idx: usize) -> Result<Option<u32>, Error> {
        if head < 2 {
            return Ok(None);
        }
        let mut visited: BTreeSet<u32> = BTreeSet::new();
        let mut cur = head;
        for _ in 0..idx {
            if !visited.insert(cur) {
                return Err(Error::Corrupted);
            }
            match self.get_fat_entry(cur)? {
                Some(next) => cur = next,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// Ensure the cluster chain rooted at `head_opt` covers logical index
    /// `target_cluster_idx`, allocating clusters as needed. Returns the (possibly
    /// new) head cluster AND the cluster at `target_cluster_idx`, so the caller
    /// does not have to re-walk via `cluster_at_index`. Does NOT patch the
    /// on-disk directory entry.
    ///
    /// A `BTreeSet<u32>` guards against cycles in existing chain.
    pub(super) fn ensure_chain_to(
        &self,
        head_opt: Option<u32>,
        target_cluster_idx: usize,
    ) -> Result<(u32, u32), Error> {
        let head = match head_opt.filter(|&h| h >= 2) {
            Some(h) => h,
            None => {
                // No head yet; allocate one (and possibly extend).
                let h = self.alloc_cluster()?;
                let mut last = h;
                for _ in 0..target_cluster_idx {
                    let nc = self.alloc_cluster()?;
                    self.link_fat_entry(last, nc)?;
                    last = nc;
                }
                return Ok((h, last));
            }
        };

        // Walk existing chain up to target_cluster_idx, extending if the chain
        // ends before that.
        let mut visited: BTreeSet<u32> = BTreeSet::new();
        let mut cur = head;
        let mut idx = 0usize;
        loop {
            if idx == target_cluster_idx {
                return Ok((head, cur));
            }
            if !visited.insert(cur) {
                return Err(Error::Corrupted);
            }
            match self.get_fat_entry(cur)? {
                Some(next) => {
                    cur = next;
                    idx += 1;
                }
                None => {
                    // Chain ends here; extend to target.
                    while idx < target_cluster_idx {
                        let nc = self.alloc_cluster()?;
                        self.link_fat_entry(cur, nc)?;
                        cur = nc;
                        idx += 1;
                    }
                    return Ok((head, cur));
                }
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Orphan eviction (called by VfsInode::drop via FileSystem::evict_inode)
    // ---------------------------------------------------------------------------

    /// Free the cluster chain associated with `ino` and drop the side-table
    /// entry. Invoked from `FileSystem::evict_inode` when the final
    /// `Arc<VfsInode>` for an orphaned FAT32 inode is released.
    ///
    /// Callers that hit this path on a still-linked (non-orphan) ino by
    /// accident will NOT lose data: remove the side-table entry only; keep
    /// the on-disk chain intact. This defensive check is cheap and prevents
    /// a double-free if the drop fires on an inode whose remove_file failed.
    pub(super) fn evict_inode_fat32(&self, ino: u64) -> Result<(), Error> {
        let entry = match self.inode_table.lock().remove(&ino) {
            Some(e) => e,
            None => return Ok(()),
        };
        if !entry.orphan {
            // Non-orphan eviction: just dropped the side-table cache entry.
            // The on-disk file is still linked; leave its chain alone.
            return Ok(());
        }
        let _guard = self.write_lock.lock();
        if entry.first_cluster >= 2 {
            self.free_cluster_chain(entry.first_cluster)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PageCacheOps impl
// ---------------------------------------------------------------------------

impl PageCacheOps for Fatfs {
    /// Read a 4 KiB page of file data from disk.
    ///
    /// Handles clusters smaller than 4 KiB (multiple clusters per page) and
    /// larger than 4 KiB (page falls inside one cluster). Returns the number of
    /// valid bytes (file bytes within the page); the caller zeros the rest.
    fn fill_page(&self, ino: u64, page_index: u64, buf: &mut [u8]) -> Result<usize, Error> {
        debug_assert_eq!(buf.len(), PAGE_SIZE);

        let entry = self.lookup_inode_entry(ino)?;
        let offset = (page_index as usize) * PAGE_SIZE;

        if offset >= entry.file_size as usize {
            buf.fill(0);
            return Ok(0);
        }

        let valid = core::cmp::min(entry.file_size as usize - offset, PAGE_SIZE);

        let bps = self.boot_info.bytes_per_sector as usize;
        let spc = self.boot_info.sectors_per_cluster as usize;
        let bpc = bps * spc; // bytes per cluster

        if entry.first_cluster < 2 {
            // No data cluster allocated; treat as a sparse zero region.
            buf[..valid].fill(0);
            buf[valid..].fill(0);
            return Ok(valid);
        }

        // Number of bytes to read from this page. We read exactly `valid` bytes
        // from disk and zero the rest of buf.
        let mut buf_pos = 0usize;
        let mut file_pos = offset;

        while buf_pos < valid {
            let cluster_idx = file_pos / bpc;
            let cluster_off = file_pos % bpc; // byte offset within the cluster

            let cluster = match self.cluster_at_index(entry.first_cluster, cluster_idx)? {
                Some(c) => c,
                None => {
                    // Past chain end before file_size: corruption (FAT32 has no holes).
                    return Err(Error::Corrupted);
                }
            };

            // How many bytes are available from this cluster, starting at cluster_off.
            let cluster_avail = bpc - cluster_off;
            // How many bytes we still need to fill in buf.
            let need = valid - buf_pos;
            let take = core::cmp::min(cluster_avail, need);

            // Compute sector-aligned read range within the cluster.
            let first_sector_in_cluster = cluster_off / SECTOR_SIZE;
            let last_byte_in_cluster = cluster_off + take - 1;
            let last_sector_in_cluster = last_byte_in_cluster / SECTOR_SIZE;
            let sectors_to_read = (last_sector_in_cluster - first_sector_in_cluster + 1) as u16;

            let lba = self.cluster_to_lba(cluster) + first_sector_in_cluster as u64;
            let read_bytes = sectors_to_read as usize * SECTOR_SIZE;

            let mut scratch = vec![0u8; read_bytes];
            let dev = block_io::lookup(self.partition.device_id).ok_or(Error::IoError)?;
            let h = dev
                .submit_read(
                    lba,
                    sectors_to_read as u32,
                    BlockBuffer::Slice {
                        ptr: scratch.as_mut_ptr(),
                        len: read_bytes,
                    },
                )
                .map_err(|_| Error::IoError)?;
            h.wait().map_err(|_| Error::IoError)?;

            // Copy the requested slice from the scratch buffer into buf.
            let src_start = cluster_off % SECTOR_SIZE;
            let src_end = src_start + take;
            buf[buf_pos..buf_pos + take].copy_from_slice(&scratch[src_start..src_end]);

            buf_pos += take;
            file_pos += take;
        }

        buf[valid..].fill(0);
        Ok(valid)
    }

    /// Write a 4 KiB page of file data to disk.
    ///
    /// The `valid_bytes` argument from VFS callers is always 4096 (see vfs.rs
    /// flush_dirty calls). Do NOT use it to determine the tail; compute real valid
    /// bytes from the side table to avoid writing zeroes past file_size into the
    /// last cluster sector.
    fn flush_page(
        &self,
        ino: u64,
        page_index: u64,
        buf: &[u8],
        _valid_bytes: usize,
    ) -> Result<(), Error> {
        debug_assert_eq!(buf.len(), PAGE_SIZE);

        let entry = self.lookup_inode_entry(ino)?;
        let offset = (page_index as usize) * PAGE_SIZE;

        // Compute real valid bytes from side table, not from _valid_bytes.
        let valid = core::cmp::min(PAGE_SIZE, (entry.file_size as usize).saturating_sub(offset));

        if valid == 0 {
            // Page is entirely beyond EOF; nothing to write.
            return Ok(());
        }

        let _guard = self.write_lock.lock();
        // Re-read entry under write lock to get up-to-date first_cluster.
        let entry = self.lookup_inode_entry(ino)?;

        let bps = self.boot_info.bytes_per_sector as usize;
        let spc = self.boot_info.sectors_per_cluster as usize;
        let bpc = bps * spc;

        // Track the actual chain head across the loop. May differ from
        // entry.first_cluster if we had to allocate the first cluster this call.
        let mut actual_head = if entry.first_cluster >= 2 {
            Some(entry.first_cluster)
        } else {
            None
        };

        let mut buf_pos = 0usize;
        let mut file_pos = offset;

        while buf_pos < valid {
            let cluster_idx = file_pos / bpc;
            let cluster_off = file_pos % bpc;

            // Ensure the cluster exists, allocating if necessary. Returns both
            // the (possibly new) head and the target cluster, avoiding a second
            // chain walk via `cluster_at_index`.
            let (head, cluster) = self.ensure_chain_to(actual_head, cluster_idx)?;

            // If the head was just allocated (first time actual_head was None),
            // patch the dirent and update the side table.
            if actual_head.is_none() {
                if !entry.orphan {
                    self.patch_dir_entry_at(
                        entry.dir_cluster,
                        entry.entry_offset as usize,
                        |de| {
                            de.first_cluster_high = ((head >> 16) & 0xFFFF) as u16;
                            de.first_cluster_low = (head & 0xFFFF) as u16;
                        },
                    )?;
                }
                if let Some(e) = self.inode_table.lock().get_mut(&ino) {
                    e.first_cluster = head;
                }
                actual_head = Some(head);
            }

            let cluster_avail = bpc - cluster_off;
            let need = valid - buf_pos;
            let take = core::cmp::min(cluster_avail, need);

            // Determine the sector-aligned write region.
            let first_sector_in_cluster = cluster_off / SECTOR_SIZE;
            let last_byte_in_cluster = cluster_off + take - 1;
            let last_sector_in_cluster = last_byte_in_cluster / SECTOR_SIZE;
            let sectors_to_write = (last_sector_in_cluster - first_sector_in_cluster + 1) as u16;
            let write_bytes = sectors_to_write as usize * SECTOR_SIZE;

            let lba = self.cluster_to_lba(cluster) + first_sector_in_cluster as u64;
            let src_start = cluster_off % SECTOR_SIZE;
            let src_end = src_start + take;

            // Build write buffer. For the partial last sector, zero-pad past EOF.
            // We never RMW: bytes past file_size in that sector are formally undefined
            // and zeros are correct per FAT32 spec.
            let mut scratch = vec![0u8; write_bytes];
            scratch[src_start..src_end].copy_from_slice(&buf[buf_pos..buf_pos + take]);

            let dev = block_io::lookup(self.partition.device_id).ok_or(Error::IoError)?;
            let h = dev
                .submit_write(
                    lba,
                    sectors_to_write as u32,
                    BlockBuffer::Slice {
                        ptr: scratch.as_mut_ptr(),
                        len: scratch.len(),
                    },
                    WriteFlags::NONE,
                )
                .map_err(|_| Error::IoError)?;
            h.wait().map_err(|_| Error::IoError)?;

            // These sectors went to the device behind the block page cache,
            // which may still hold them from a directory or format read. Drop
            // its copy so a later writeback cannot put it back on top.
            let first_page = lba / 8;
            let last_page = (lba + sectors_to_write as u64 - 1) / 8;
            crate::fs::block_page_cache::BlockPageCache::global().invalidate_pages(
                self.partition.device_id,
                first_page,
                last_page - first_page + 1,
            );

            buf_pos += take;
            file_pos += take;
        }

        Ok(())
    }

    /// Update the on-disk file size. Grow-only: no-op if `new_size <= current size`.
    ///
    /// Shrinking is the responsibility of `FileSystem::truncate`. This asymmetry is
    /// intentional: the page cache calls `update_size` after a page write to record
    /// the new EOF, but never to shrink (truncate does that explicitly).
    fn update_size(&self, ino: u64, new_size: u64) -> Result<(), Error> {
        if new_size > u32::MAX as u64 {
            return Err(Error::IoError);
        }

        // Pre-check without the write lock to avoid unnecessary contention on
        // the grow-only no-op path.
        let pre = self.lookup_inode_entry(ino)?;
        if new_size <= pre.file_size as u64 {
            return Ok(());
        }

        let _guard = self.write_lock.lock();

        // Re-read under write_lock. remove_file also acquires write_lock before
        // flipping orphan=true, so this catches a concurrent unlink and avoids
        // writing the new size into a dirent that was just marked 0xE5.
        let entry = self.lookup_inode_entry(ino)?;
        if new_size <= entry.file_size as u64 {
            return Ok(());
        }

        // Skip dirent patch for orphaned inodes (dirent already marked 0xE5).
        if !entry.orphan {
            let current_time = FileTime::now();
            self.patch_dir_entry_at(entry.dir_cluster, entry.entry_offset as usize, |de| {
                de.file_size = new_size as u32;
                de.attributes |= 0x20; // ARCHIVE
                de.write_date = current_time.date;
                de.write_time = current_time.time;
            })?;
        }

        // Update the side table.
        if let Some(e) = self.inode_table.lock().get_mut(&ino) {
            e.file_size = new_size as u32;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FileSystem trait overrides
// ---------------------------------------------------------------------------

impl Fatfs {
    /// Return PageCacheOps only for FAT32. FAT12/16 fall back to path-based I/O.
    ///
    /// Returning Some for FAT12/16 would surface EIO to userspace because those
    /// variants do not populate the inode_table (lookup_inode_entry would return
    /// FileNotFound on every fill_page/flush_page call).
    pub(super) fn as_page_cache_ops_fat32(&self) -> Option<&dyn PageCacheOps> {
        match self.variant {
            crate::fs::fat32::structures::FatVariant::Fat32 => Some(self),
            _ => None,
        }
    }

    /// Return the file size from the side table (hot path for fault.rs past-EOF check).
    pub(super) fn file_size_ino_fat32(&self, ino: u64) -> Result<u64, Error> {
        self.lookup_inode_entry(ino).map(|e| e.file_size as u64)
    }
}
