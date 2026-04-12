// EFS kernel driver.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use efs_common::{
    DIR_ENTRY_HEADER_SIZE, EFS_ROOT_INO, EXTENT_MAGIC, EfsBlockGroupDesc, EfsDirEntryHeader,
    EfsExtent, EfsExtentHeader, EfsInode, EfsSuperblock, FT_DIR, FT_REG_FILE, INCOMPAT_JOURNAL,
    INODE_DATA_AREA_SIZE, INODE_FLAG_INLINE_DATA, JOURNAL_MAGIC, JournalSuperblock,
    MAX_INLINE_EXTENTS, S_IFDIR, S_IFMT, S_IFREG, checksum_inode, dir_entry_min_size,
    journal_sb_checksum,
};

use super::block_device::BlockDevice;
use super::block_page_cache::BlockPageCache;
use super::gpt::Partition;
use super::journal::{Journal, tx::TxHandle};
use super::page_cache::PageCacheOps;
use super::path::Path;
use super::{Error, File, FileAttrs, FileKind, FileSystem, FileTime};
use crate::log;
use crate::thread::mutex::BlockingMutex;

// ---- Constants ----------------------------------------------------------------

/// Number of 512-byte sectors in 4 KiB (one default block).
const SECTORS_PER_DEFAULT_BLOCK: u16 = 8;

/// Size of one block group descriptor on disk.
const BGD_SIZE: usize = core::mem::size_of::<EfsBlockGroupDesc>();

/// Size of one inode on disk.
const INODE_SIZE: usize = core::mem::size_of::<EfsInode>();

/// Stack-allocated extent list (max 13 extents, no heap allocation).
#[derive(Clone)]
struct ExtentList {
    extents: [EfsExtent; MAX_INLINE_EXTENTS],
    len: usize,
}

impl ExtentList {
    fn as_slice(&self) -> &[EfsExtent] {
        &self.extents[..self.len]
    }

    fn as_mut_slice(&mut self) -> &mut [EfsExtent] {
        &mut self.extents[..self.len]
    }

    fn push(&mut self, ext: EfsExtent) -> bool {
        if self.len < MAX_INLINE_EXTENTS {
            self.extents[self.len] = ext;
            self.len += 1;
            true
        } else {
            false
        }
    }
}

// ---- Driver struct ------------------------------------------------------------

/// Mutable filesystem metadata protected by a lock.
struct EfsMutableState {
    superblock: EfsSuperblock,
    bgd_table: Vec<EfsBlockGroupDesc>,
}

pub struct EfsDriver {
    device: BlockDevice,
    partition: Partition,
    /// Cached values derived from the superblock (immutable after mount).
    block_size_log2: u32,
    inodes_per_group: u32,
    /// Live journal for this filesystem.
    journal: Arc<Journal>,
    /// Mutable FS metadata (superblock + block group descriptors).
    mutable: BlockingMutex<EfsMutableState>,
}

// ---- Constructor --------------------------------------------------------------

impl EfsDriver {
    pub fn new(partition: Partition) -> Result<Self, Error> {
        let device = BlockDevice::new(partition.device_id);

        // Block 0 = boot/reserved, block 1 = superblock.
        // sb_lba is always partition.starting_lba + 8, which is a 4 KiB page boundary.
        let sb_lba = partition.starting_lba + SECTORS_PER_DEFAULT_BLOCK as u64;
        let sb_page = device.read_page(sb_lba / SECTORS_PER_DEFAULT_BLOCK as u64)?;
        let sb_bytes = sb_page.as_slice();

        // SAFETY: EfsSuperblock is repr(C, packed), 256 bytes; the buffer is
        // at least 256 bytes.  We use read_unaligned to avoid UB on packed fields.
        let superblock: EfsSuperblock =
            unsafe { core::ptr::read_unaligned(sb_bytes.as_ptr() as *const EfsSuperblock) };

        superblock.validate().map_err(|_| Error::InvalidFs)?;

        // Refuse to mount without a journal. Filesystems formatted with an
        // older efs-mkfs that lacks INCOMPAT_JOURNAL must be reformatted.
        if superblock.incompatible_features & INCOMPAT_JOURNAL == 0 {
            log!(
                "efs: refusing to mount -- journal required (INCOMPAT_JOURNAL missing). \
                 Reformat with latest efs-mkfs."
            );
            return Err(Error::InvalidFs);
        }

        let block_size = 1u64 << superblock.block_size_log2;
        // BlockPageCache requires 4 KiB blocks (one block == one page).
        if block_size != 4096 {
            return Err(Error::InvalidFs);
        }
        let sectors_per_block = (block_size / 512) as u16;
        let starting_lba = partition.starting_lba;

        // Block 2 = start of BGD table.
        // bgd_lba = starting_lba + 16, which is a 4 KiB page boundary.
        let bgd_lba = starting_lba + 2 * (block_size / 512);
        let bgd_count = superblock.block_group_count as usize;
        // How many pages do we need for the BGD table?
        let bgd_bytes_needed = bgd_count * BGD_SIZE;
        let bgd_pages = ((bgd_bytes_needed + 4095) / 4096).max(1);
        let bgd_page_guards = device.read_pages(bgd_lba / sectors_per_block as u64, bgd_pages)?;
        // Flatten pages into a contiguous slice for parsing.
        let bgd_flat: Vec<u8> = bgd_page_guards
            .iter()
            .flat_map(|g| g.as_slice().iter().copied())
            .collect();

        let mut bgd_table = Vec::with_capacity(bgd_count);
        for i in 0..bgd_count {
            let offset = i * BGD_SIZE;
            let bgd: EfsBlockGroupDesc = unsafe {
                core::ptr::read_unaligned(bgd_flat[offset..].as_ptr() as *const EfsBlockGroupDesc)
            };
            bgd_table.push(bgd);
        }

        // ---- Validate journal superblock ----------------------------------------
        // journal_first_block is an absolute EFS block number (0 = first block of
        // the partition). Convert to page index the same way read_block does:
        //   page_idx = lba / SECTORS_PER_DEFAULT_BLOCK
        //   lba = starting_lba + block * (block_size / 512)
        let j_first_block = superblock.journal_first_block;
        let j_lba = starting_lba + j_first_block * sectors_per_block as u64;
        let j_page_idx = j_lba / SECTORS_PER_DEFAULT_BLOCK as u64;
        let j_page = device.read_page(j_page_idx)?;
        let j_bytes = j_page.as_slice();

        // SAFETY: JournalSuperblock is repr(C, packed), 64 bytes; the page is 4096 bytes.
        let jsb: JournalSuperblock =
            unsafe { core::ptr::read_unaligned(j_bytes.as_ptr() as *const JournalSuperblock) };

        // Copy packed fields to locals to avoid misaligned reference UB.
        let jsb_magic = jsb.magic;
        let jsb_version = jsb.version;
        let jsb_crc32 = jsb.crc32;
        let jsb_block_count = jsb.block_count;
        let jsb_head_seq = jsb.head_seq;
        let jsb_tail_seq = jsb.tail_seq;

        if jsb_magic != JOURNAL_MAGIC {
            log!("efs: journal superblock has bad magic {:#x}", jsb_magic);
            return Err(Error::InvalidFs);
        }
        if jsb_version != 1 {
            log!(
                "efs: journal superblock has unsupported version {}",
                jsb_version
            );
            return Err(Error::InvalidFs);
        }
        let expected_crc = journal_sb_checksum(&jsb);
        if jsb_crc32 != expected_crc {
            log!(
                "efs: journal superblock checksum mismatch (got {:#x}, expected {:#x})",
                jsb_crc32,
                expected_crc
            );
            return Err(Error::InvalidFs);
        }

        let block_size_log2 = superblock.block_size_log2;
        let inodes_per_group = superblock.inodes_per_group;

        // j_first_block is already a partition-relative EFS block number.
        let journal = Journal::new(
            partition.device_id,
            j_first_block,
            jsb_block_count,
            jsb_head_seq,
            jsb_tail_seq,
        );

        // Register the journal with the block page cache so writeback can
        // gate flushes on commit state.
        BlockPageCache::global().register_device(partition.device_id, journal.clone());

        Ok(Self {
            device,
            partition,
            block_size_log2,
            inodes_per_group,
            journal,
            mutable: BlockingMutex::new(EfsMutableState {
                superblock,
                bgd_table,
            }),
        })
    }
}

// ---- Low-level block/inode helpers -------------------------------------------

impl EfsDriver {
    fn block_size(&self) -> u64 {
        1u64 << self.block_size_log2
    }

    fn sectors_per_block(&self) -> u16 {
        (self.block_size() / 512) as u16
    }

    fn block_to_lba(&self, block: u64) -> u64 {
        self.partition.starting_lba + block * (self.block_size() / 512)
    }

    fn read_block(&self, block: u64) -> Result<Vec<u8>, Error> {
        let lba = self.block_to_lba(block);
        let page_idx = lba / 8;
        let guard = self.device.read_page(page_idx)?;
        // .to_vec() copies from the pinned frame; callers currently expect Vec<u8>.
        // When guard signatures are propagated upward this copy can be removed.
        Ok(guard.as_slice().to_vec())
    }

    fn write_block(&self, block: u64, data: &[u8], tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let lba = self.block_to_lba(block);
        let page_idx = lba / 8;
        let mut buf = [0u8; 4096];
        let n = data.len().min(4096);
        buf[..n].copy_from_slice(&data[..n]);
        self.device.write_page(page_idx, &buf)?;
        // Enroll the freshly written page in the transaction.
        let guard = BlockPageCache::global()
            .read_page(self.device.device_id, page_idx)
            .map_err(|_| Error::IoError)?;
        tx.enroll_block(self.device.device_id, page_idx, guard.page_arc());
        Ok(())
    }

    /// Map inode number to (group_index, inode_index_within_group).
    fn inode_location(&self, ino: u64) -> (usize, usize) {
        let ino0 = (ino - 1) as usize; // inodes are 1-based
        let ipg = self.inodes_per_group as usize;
        (ino0 / ipg, ino0 % ipg)
    }

    // Inode-table blocks hit the block page cache after first access.
    fn read_inode(&self, ino: u64) -> Result<EfsInode, Error> {
        let (group, idx) = self.inode_location(ino);
        let inode_table_block = {
            let m = self.mutable.lock();
            if group >= m.bgd_table.len() {
                return Err(Error::Corrupted);
            }
            m.bgd_table[group].inode_table_block
        };
        let block_size = self.block_size() as usize;
        let inodes_per_block = block_size / INODE_SIZE;
        let block_offset = idx / inodes_per_block;
        let offset_in_block = (idx % inodes_per_block) * INODE_SIZE;

        let block_data = self.read_block(inode_table_block + block_offset as u64)?;
        let inode: EfsInode = unsafe {
            core::ptr::read_unaligned(block_data[offset_in_block..].as_ptr() as *const EfsInode)
        };
        Ok(inode)
    }

    fn write_inode(&self, ino: u64, inode: &EfsInode, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let (group, idx) = self.inode_location(ino);
        let inode_table_block = {
            let m = self.mutable.lock();
            if group >= m.bgd_table.len() {
                return Err(Error::Corrupted);
            }
            m.bgd_table[group].inode_table_block
        };
        let block_size = self.block_size() as usize;
        let inodes_per_block = block_size / INODE_SIZE;
        let block_offset = idx / inodes_per_block;
        let offset_in_block = (idx % inodes_per_block) * INODE_SIZE;

        let mut block_data = self.read_block(inode_table_block + block_offset as u64)?;
        let dst = &mut block_data[offset_in_block..offset_in_block + INODE_SIZE];
        let src: &[u8] = unsafe {
            core::slice::from_raw_parts(inode as *const EfsInode as *const u8, INODE_SIZE)
        };
        dst.copy_from_slice(src);
        self.write_block(inode_table_block + block_offset as u64, &block_data, tx)?;
        Ok(())
    }
}

// ---- File data reading --------------------------------------------------------

impl EfsDriver {
    /// Read up to `count` bytes from a file inode starting at `offset`.
    fn read_file_data(
        &self,
        inode: &EfsInode,
        offset: usize,
        count: usize,
    ) -> Result<Vec<u8>, Error> {
        let file_size = inode.size as usize;
        if offset >= file_size || count == 0 {
            return Ok(vec![]);
        }
        let available = file_size - offset;
        let to_read = count.min(available);

        if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            // Data lives directly in data_area.
            let end = (offset + to_read).min(inode.data_area.len());
            return Ok(inode.data_area[offset..end].to_vec());
        }

        // Extent tree (depth 0 only for v1).
        self.read_via_extents(inode, offset, to_read)
    }

    fn read_via_extents(
        &self,
        inode: &EfsInode,
        byte_offset: usize,
        count: usize,
    ) -> Result<Vec<u8>, Error> {
        let hdr: EfsExtentHeader = unsafe {
            core::ptr::read_unaligned(inode.data_area.as_ptr() as *const EfsExtentHeader)
        };
        if hdr.magic != EXTENT_MAGIC {
            return Err(Error::Corrupted);
        }
        if hdr.depth != 0 {
            // v1 only supports depth-0 (flat extent list).
            return Err(Error::Unsupported);
        }

        let extents = self.parse_inline_extents(&inode.data_area, hdr.entries as usize)?;
        let ext_slice = extents.as_slice();
        let block_size = self.block_size() as usize;
        let spb = self.sectors_per_block();

        let mut result = vec![0u8; count];
        let mut result_pos = 0usize;
        let mut remaining = count;
        let mut cur_byte = byte_offset;
        let mut ext_idx = 0usize;

        while remaining > 0 {
            let logical_block = (cur_byte / block_size) as u32;
            let offset_in_block = cur_byte % block_size;

            // Find the extent covering logical_block.
            let extent = ext_slice[ext_idx..]
                .iter()
                .chain(ext_slice[..ext_idx].iter())
                .enumerate()
                .find(|(_, e)| {
                    e.logical_block <= logical_block
                        && logical_block < e.logical_block + e.length as u32
                });

            let extent = match extent {
                Some((i, e)) => {
                    ext_idx = (ext_idx + i) % ext_slice.len();
                    e
                }
                None => return Err(Error::Corrupted),
            };

            let block_within_extent = logical_block - extent.logical_block;
            let blocks_left_in_extent = extent.length as u32 - block_within_extent;

            // How many contiguous blocks can we read in one shot?
            // Cap at per-slot pool size (248 pages = 992KB) per AHCI command.
            // With NCQ, multiple commands can be in flight concurrently.
            const MAX_BULK_BLOCKS: u32 = 248;
            let blocks_needed =
                ((remaining + offset_in_block + block_size - 1) / block_size) as u32;
            let bulk_blocks = blocks_needed
                .min(blocks_left_in_extent)
                .min(MAX_BULK_BLOCKS);

            let phys_block = extent.physical_start() + block_within_extent as u64;
            let lba = self.block_to_lba(phys_block);
            let total_sectors = (bulk_blocks as u32 * spb as u32) as u16;

            // INVARIANT: file-data reads bypass BlockDevice to avoid shredding
            // bulk AHCI commands into per-page cache ops. The per-inode page
            // cache owns file data — do not route through BlockPageCache.
            let mut bulk_data = vec![0u8; total_sectors as usize * 512];
            crate::drivers::ahci::direct::read_sectors(
                self.device.device_id,
                lba,
                total_sectors,
                &mut bulk_data,
            )?;

            // Copy the useful portion into result.
            let bulk_bytes = bulk_blocks as usize * block_size;
            let available = bulk_bytes - offset_in_block;
            let copy_len = remaining.min(available);
            result[result_pos..result_pos + copy_len]
                .copy_from_slice(&bulk_data[offset_in_block..offset_in_block + copy_len]);

            result_pos += copy_len;
            remaining -= copy_len;
            cur_byte += copy_len;
        }

        Ok(result)
    }

    fn parse_inline_extents(
        &self,
        data_area: &[u8; INODE_DATA_AREA_SIZE],
        count: usize,
    ) -> Result<ExtentList, Error> {
        let header_size = core::mem::size_of::<EfsExtentHeader>();
        let extent_size = core::mem::size_of::<EfsExtent>();
        let max_count = (INODE_DATA_AREA_SIZE - header_size) / extent_size;
        let count = count.min(max_count).min(MAX_INLINE_EXTENTS);

        let mut list = ExtentList {
            extents: [EfsExtent {
                logical_block: 0,
                length: 0,
                start_hi: 0,
                start_lo: 0,
            }; MAX_INLINE_EXTENTS],
            len: count,
        };
        for i in 0..count {
            let offset = header_size + i * extent_size;
            list.extents[i] = unsafe {
                core::ptr::read_unaligned(data_area[offset..].as_ptr() as *const EfsExtent)
            };
        }
        Ok(list)
    }
}

// ---- Directory operations -----------------------------------------------------

impl EfsDriver {
    /// Return (name, inode_num, file_type) for all valid entries in a directory.
    fn read_dir_entries(&self, ino: u64) -> Result<Vec<(String, u64, u8)>, Error> {
        let inode = self.read_inode(ino)?;
        if inode.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }
        // Directory data is metadata — read through the block page cache so
        // we see dirty pages written by add_dir_entry/remove_dir_entry (which
        // go through write_block → BlockPageCache in write-back mode).
        // Do NOT use read_file_data/read_via_extents here; those bypass the
        // cache via direct::read_sectors and would miss uncommitted changes.
        let dir_data = self.read_dir_data_cached(&inode)?;
        self.parse_dir_entries_from_bytes(&dir_data)
    }

    fn read_dir_data_cached(&self, inode: &EfsInode) -> Result<Vec<u8>, Error> {
        let dir_size = inode.size as usize;
        if dir_size == 0 {
            return Ok(vec![]);
        }
        if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            return Ok(inode.data_area[..dir_size.min(INODE_DATA_AREA_SIZE)].to_vec());
        }
        let block_size = self.block_size() as usize;
        let hdr: EfsExtentHeader = unsafe {
            core::ptr::read_unaligned(inode.data_area.as_ptr() as *const EfsExtentHeader)
        };
        if hdr.magic != EXTENT_MAGIC || hdr.depth != 0 {
            return Err(Error::Corrupted);
        }
        let extents = self.parse_inline_extents(&inode.data_area, hdr.entries as usize)?;
        let mut result = vec![0u8; dir_size];
        let mut result_pos = 0usize;
        for ext in extents.as_slice() {
            for i in 0..ext.length as u64 {
                if result_pos >= dir_size {
                    break;
                }
                let phys_block = ext.physical_start() + i;
                let block_data = self.read_block(phys_block)?;
                let copy_len = (dir_size - result_pos).min(block_size);
                result[result_pos..result_pos + copy_len].copy_from_slice(&block_data[..copy_len]);
                result_pos += copy_len;
            }
        }
        Ok(result)
    }

    fn parse_dir_entries_from_bytes(&self, data: &[u8]) -> Result<Vec<(String, u64, u8)>, Error> {
        let mut entries = Vec::new();
        let mut offset = 0usize;

        while offset + DIR_ENTRY_HEADER_SIZE <= data.len() {
            let hdr: EfsDirEntryHeader = unsafe {
                core::ptr::read_unaligned(data[offset..].as_ptr() as *const EfsDirEntryHeader)
            };
            let rec_len = hdr.rec_len as usize;
            if rec_len < DIR_ENTRY_HEADER_SIZE || offset + rec_len > data.len() {
                break;
            }
            if hdr.inode != 0 {
                let name_start = offset + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + hdr.name_len as usize;
                if name_end <= data.len() {
                    let name_bytes = &data[name_start..name_end];
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        entries.push((name.to_string(), hdr.inode, hdr.file_type));
                    }
                }
            }
            offset += rec_len;
        }
        Ok(entries)
    }

    /// Look up a single entry by name in a directory, without allocating Strings.
    /// Returns (inode, file_type) on match.
    fn lookup_in_dir(&self, dir_ino: u64, name: &str) -> Result<Option<(u64, u8)>, Error> {
        let inode = self.read_inode(dir_ino)?;
        if inode.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }
        let dir_data = self.read_dir_data_cached(&inode)?;
        let name_bytes = name.as_bytes();
        let mut offset = 0usize;

        while offset + DIR_ENTRY_HEADER_SIZE <= dir_data.len() {
            let hdr: EfsDirEntryHeader = unsafe {
                core::ptr::read_unaligned(dir_data[offset..].as_ptr() as *const EfsDirEntryHeader)
            };
            let rec_len = hdr.rec_len as usize;
            if rec_len < DIR_ENTRY_HEADER_SIZE || offset + rec_len > dir_data.len() {
                break;
            }
            if hdr.inode != 0 && hdr.name_len as usize == name_bytes.len() {
                let ns = offset + DIR_ENTRY_HEADER_SIZE;
                let ne = ns + hdr.name_len as usize;
                if ne <= dir_data.len() && &dir_data[ns..ne] == name_bytes {
                    return Ok(Some((hdr.inode, hdr.file_type)));
                }
            }
            offset += rec_len;
        }
        Ok(None)
    }

    /// Resolve a path to its inode number.
    fn resolve_path(&self, path: &Path) -> Result<u64, Error> {
        Ok(self.resolve_path_inode(path)?.0)
    }

    /// Resolve a path to (inode_number, inode), avoiding a redundant read_inode after resolution.
    fn resolve_path_inode(&self, path: &Path) -> Result<(u64, EfsInode), Error> {
        if path.is_root() {
            let inode = self.read_inode(EFS_ROOT_INO)?;
            return Ok((EFS_ROOT_INO, inode));
        }

        let mut current_ino = EFS_ROOT_INO;
        for component in path.components() {
            match self.lookup_in_dir(current_ino, component.as_str())? {
                Some((ino, _)) => current_ino = ino,
                None => return Err(Error::FileNotFound),
            }
        }
        let inode = self.read_inode(current_ino)?;
        Ok((current_ino, inode))
    }
}

// ---- Write path: file data ----------------------------------------------------

impl EfsDriver {
    /// Write `data` to an inode starting at `byte_offset`.
    /// Grows the file if necessary.  Returns the new file size.
    fn write_file_data(
        &self,
        ino: u64,
        byte_offset: usize,
        data: &[u8],
        tx: &mut TxHandle<'_>,
    ) -> Result<u64, Error> {
        if data.is_empty() {
            return Ok(self.read_inode(ino)?.size);
        }

        let inode = self.read_inode(ino)?;
        let end_offset = byte_offset + data.len();
        let new_size = (end_offset as u64).max(inode.size);

        if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            // Can we still fit inline?
            if end_offset <= INODE_DATA_AREA_SIZE {
                return self.write_inline(ino, &inode, byte_offset, data, new_size, tx);
            }
            // Must convert to extent mode before writing.
            self.convert_inline_to_extents(ino, &inode, tx)?;
        }

        self.write_via_extents(ino, byte_offset, data, tx)?;
        let mut updated = self.read_inode(ino)?;
        if new_size > updated.size {
            updated.size = new_size;
        }
        updated.mtime_sec = current_unix_time();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated, tx)?;
        Ok(updated.size)
    }

    fn write_inline(
        &self,
        ino: u64,
        inode: &EfsInode,
        offset: usize,
        data: &[u8],
        new_size: u64,
        tx: &mut TxHandle<'_>,
    ) -> Result<u64, Error> {
        let mut updated = *inode;
        updated.data_area[offset..offset + data.len()].copy_from_slice(data);
        updated.size = new_size;
        updated.mtime_sec = current_unix_time();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated, tx)?;
        Ok(new_size)
    }

    fn convert_inline_to_extents(
        &self,
        ino: u64,
        inode: &EfsInode,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let inline_data = inode.data_area[..inode.size as usize].to_vec();
        let block_size = self.block_size() as usize;

        // Allocate one block for the data.
        let phys_block = self.alloc_block(tx)?;
        let mut block_buf = vec![0u8; block_size];
        block_buf[..inline_data.len()].copy_from_slice(&inline_data);
        self.write_block(phys_block, &block_buf, tx)?;

        // Build extent header + one extent in data_area.
        let mut updated = *inode;
        updated.flags &= !INODE_FLAG_INLINE_DATA;
        updated.data_area = [0u8; INODE_DATA_AREA_SIZE];

        let hdr = EfsExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 1,
            max_entries: efs_common::MAX_INLINE_EXTENTS as u16,
            depth: 0,
            reserved: 0,
        };
        let hdr_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &hdr as *const EfsExtentHeader as *const u8,
                core::mem::size_of::<EfsExtentHeader>(),
            )
        };
        updated.data_area[..hdr_bytes.len()].copy_from_slice(hdr_bytes);

        let ext = EfsExtent {
            logical_block: 0,
            length: 1,
            start_hi: (phys_block >> 32) as u16,
            start_lo: phys_block as u32,
        };
        let ext_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &ext as *const EfsExtent as *const u8,
                core::mem::size_of::<EfsExtent>(),
            )
        };
        let hdr_size = core::mem::size_of::<EfsExtentHeader>();
        updated.data_area[hdr_size..hdr_size + ext_bytes.len()].copy_from_slice(ext_bytes);

        updated.blocks = 1;
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated, tx)
    }

    fn write_via_extents(
        &self,
        ino: u64,
        byte_offset: usize,
        data: &[u8],
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let block_size = self.block_size() as usize;
        let spb = self.sectors_per_block();
        let mut written = 0usize;

        while written < data.len() {
            let cur_byte = byte_offset + written;
            let logical_block = (cur_byte / block_size) as u32;
            let offset_in_block = cur_byte % block_size;
            let copy_len = (data.len() - written).min(block_size - offset_in_block);

            let phys_block = self.ensure_block_for_logical(ino, logical_block, tx)?;

            // INVARIANT: file-data writes bypass BlockPageCache to stay consistent
            // with the read path (read_via_extents) which also bypasses it. Only
            // metadata (inode, bitmap, BGD) goes through the journaled block cache.
            let lba = self.block_to_lba(phys_block);
            let mut block_data = vec![0u8; block_size];
            crate::drivers::ahci::direct::read_sectors(
                self.device.device_id,
                lba,
                spb,
                &mut block_data,
            )?;
            block_data[offset_in_block..offset_in_block + copy_len]
                .copy_from_slice(&data[written..written + copy_len]);
            crate::drivers::ahci::direct::write_sectors(
                self.device.device_id,
                lba,
                &block_data,
                spb,
            )?;

            written += copy_len;
        }
        Ok(())
    }

    /// Return the physical block for the given logical block, allocating if needed.
    fn ensure_block_for_logical(
        &self,
        ino: u64,
        logical_block: u32,
        tx: &mut TxHandle<'_>,
    ) -> Result<u64, Error> {
        let inode = self.read_inode(ino)?;
        let hdr: EfsExtentHeader = unsafe {
            core::ptr::read_unaligned(inode.data_area.as_ptr() as *const EfsExtentHeader)
        };
        if hdr.magic != EXTENT_MAGIC || hdr.depth != 0 {
            return Err(Error::Corrupted);
        }

        let extents = self.parse_inline_extents(&inode.data_area, hdr.entries as usize)?;

        // Check if already mapped.
        for ext in extents.as_slice() {
            if ext.logical_block <= logical_block
                && logical_block < ext.logical_block + ext.length as u32
            {
                return Ok(ext.physical_start() + (logical_block - ext.logical_block) as u64);
            }
        }

        // Allocate a new block.
        let phys_block = self.alloc_block(tx)?;
        // Zero it.
        let block_size = self.block_size() as usize;
        self.write_block(phys_block, &vec![0u8; block_size], tx)?;

        // Can we extend an existing extent?
        let mut new_extents = extents.clone();
        let mut extended = false;
        for ext in new_extents.as_mut_slice().iter_mut() {
            if ext.logical_block + ext.length as u32 == logical_block
                && ext.physical_start() + ext.length as u64 == phys_block
            {
                ext.length += 1;
                extended = true;
                break;
            }
        }

        if !extended {
            if !new_extents.push(EfsExtent {
                logical_block,
                length: 1,
                start_hi: (phys_block >> 32) as u16,
                start_lo: phys_block as u32,
            }) {
                return Err(Error::Unsupported);
            }
        }

        // Write back updated extents into inode.
        let mut updated = inode;
        let hdr_size = core::mem::size_of::<EfsExtentHeader>();
        let ext_size = core::mem::size_of::<EfsExtent>();
        let new_hdr = EfsExtentHeader {
            magic: EXTENT_MAGIC,
            entries: new_extents.len as u16,
            max_entries: efs_common::MAX_INLINE_EXTENTS as u16,
            depth: 0,
            reserved: 0,
        };
        let hdr_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(&new_hdr as *const EfsExtentHeader as *const u8, hdr_size)
        };
        updated.data_area[..hdr_size].copy_from_slice(hdr_bytes);
        for (i, ext) in new_extents.as_slice().iter().enumerate() {
            let off = hdr_size + i * ext_size;
            let ext_bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(ext as *const EfsExtent as *const u8, ext_size)
            };
            updated.data_area[off..off + ext_size].copy_from_slice(ext_bytes);
        }
        updated.blocks = new_extents.as_slice().iter().map(|e| e.length as u64).sum();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated, tx)?;

        Ok(phys_block)
    }
}

// ---- Bitmap operations --------------------------------------------------------

impl EfsDriver {
    /// Allocate a free block and return its absolute block number.
    fn alloc_block(&self, tx: &mut TxHandle<'_>) -> Result<u64, Error> {
        let block_size = self.block_size() as usize;
        let mut m = self.mutable.lock();
        let blocks_per_group = m.superblock.blocks_per_group as usize;

        for g in 0..m.bgd_table.len() {
            if m.bgd_table[g].free_blocks_count == 0 {
                continue;
            }
            let bitmap_block = m.bgd_table[g].block_bitmap_block;
            let mut bitmap = self.read_block(bitmap_block)?;

            let bits_to_check = blocks_per_group.min(block_size * 8);
            if let Some(bit) = find_free_bit(&bitmap, bits_to_check) {
                set_bit(&mut bitmap, bit);
                m.bgd_table[g].free_blocks_count -= 1;
                m.superblock.free_blocks -= 1;

                let abs_block = g as u64 * blocks_per_group as u64 + bit as u64;

                // Write bitmap (enrolled by write_block).
                drop(m);
                self.write_block(bitmap_block, &bitmap, tx)?;

                // Enroll BGD page (block 2 contains the BGD table; compute the
                // page holding group g's descriptor).
                let bgd_page_idx = {
                    let bgds_per_block = block_size / BGD_SIZE;
                    let bgd_block = 2u64 + (g / bgds_per_block) as u64;
                    self.block_to_lba(bgd_block) / 8
                };
                if let Ok(guard) =
                    BlockPageCache::global().read_page(self.device.device_id, bgd_page_idx)
                {
                    tx.enroll_block(self.device.device_id, bgd_page_idx, guard.page_arc());
                }

                // Enroll superblock page (block 1).
                let sb_page_idx = self.block_to_lba(1) / 8;
                if let Ok(guard) =
                    BlockPageCache::global().read_page(self.device.device_id, sb_page_idx)
                {
                    tx.enroll_block(self.device.device_id, sb_page_idx, guard.page_arc());
                }

                return Ok(abs_block);
            }
        }
        Err(Error::IoError)
    }

    /// Free a block (by absolute block number).
    fn free_block(&self, block: u64, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let block_size = self.block_size() as usize;
        let (group, bit, bitmap_block) = {
            let m = self.mutable.lock();
            let blocks_per_group = m.superblock.blocks_per_group as u64;
            let group = (block / blocks_per_group) as usize;
            let bit = (block % blocks_per_group) as usize;
            if group >= m.bgd_table.len() {
                return Err(Error::Corrupted);
            }
            (group, bit, m.bgd_table[group].block_bitmap_block)
        };

        let mut bitmap = self.read_block(bitmap_block)?;
        clear_bit(&mut bitmap, bit);
        self.write_block(bitmap_block, &bitmap, tx)?;

        {
            let mut m = self.mutable.lock();
            m.bgd_table[group].free_blocks_count += 1;
            m.superblock.free_blocks += 1;
        }

        // Enroll BGD page.
        let bgd_page_idx = {
            let bgds_per_block = block_size / BGD_SIZE;
            let bgd_block = 2u64 + (group / bgds_per_block) as u64;
            self.block_to_lba(bgd_block) / 8
        };
        if let Ok(guard) = BlockPageCache::global().read_page(self.device.device_id, bgd_page_idx) {
            tx.enroll_block(self.device.device_id, bgd_page_idx, guard.page_arc());
        }

        // Enroll superblock page (block 1).
        let sb_page_idx = self.block_to_lba(1) / 8;
        if let Ok(guard) = BlockPageCache::global().read_page(self.device.device_id, sb_page_idx) {
            tx.enroll_block(self.device.device_id, sb_page_idx, guard.page_arc());
        }

        // Revoke the freed block so replay doesn't overwrite freed space.
        // The freed block's partition-relative page index:
        let freed_page_idx = self.block_to_lba(block) / 8;
        tx.enroll_revoke(self.device.device_id, freed_page_idx);

        Ok(())
    }

    /// Allocate a free inode and return its inode number (1-based).
    fn alloc_inode(&self, tx: &mut TxHandle<'_>) -> Result<u64, Error> {
        let block_size = self.block_size() as usize;
        let mut m = self.mutable.lock();
        let inodes_per_group = m.superblock.inodes_per_group as usize;

        for g in 0..m.bgd_table.len() {
            if m.bgd_table[g].free_inodes_count == 0 {
                continue;
            }
            let bitmap_block = m.bgd_table[g].inode_bitmap_block;
            let mut bitmap = self.read_block(bitmap_block)?;

            let bits_to_check = inodes_per_group.min(block_size * 8);
            if let Some(bit) = find_free_bit(&bitmap, bits_to_check) {
                set_bit(&mut bitmap, bit);
                m.bgd_table[g].free_inodes_count -= 1;
                m.superblock.free_inodes -= 1;

                let ino = g as u64 * inodes_per_group as u64 + bit as u64 + 1;
                drop(m);

                self.write_block(bitmap_block, &bitmap, tx)?;

                // Enroll BGD page.
                let bgd_page_idx = {
                    let bgds_per_block = block_size / BGD_SIZE;
                    let bgd_block = 2u64 + (g / bgds_per_block) as u64;
                    self.block_to_lba(bgd_block) / 8
                };
                if let Ok(guard) =
                    BlockPageCache::global().read_page(self.device.device_id, bgd_page_idx)
                {
                    tx.enroll_block(self.device.device_id, bgd_page_idx, guard.page_arc());
                }

                // Enroll superblock page (block 1).
                let sb_page_idx = self.block_to_lba(1) / 8;
                if let Ok(guard) =
                    BlockPageCache::global().read_page(self.device.device_id, sb_page_idx)
                {
                    tx.enroll_block(self.device.device_id, sb_page_idx, guard.page_arc());
                }

                return Ok(ino);
            }
        }
        Err(Error::IoError)
    }

    /// Free an inode.
    fn free_inode(&self, ino: u64, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let block_size = self.block_size() as usize;
        let inodes_per_group = self.inodes_per_group as usize;
        let ino0 = (ino - 1) as usize;
        let group = ino0 / inodes_per_group;
        let bit = ino0 % inodes_per_group;

        let bitmap_block = {
            let m = self.mutable.lock();
            if group >= m.bgd_table.len() {
                return Err(Error::Corrupted);
            }
            m.bgd_table[group].inode_bitmap_block
        };

        let mut bitmap = self.read_block(bitmap_block)?;
        clear_bit(&mut bitmap, bit);
        self.write_block(bitmap_block, &bitmap, tx)?;

        {
            let mut m = self.mutable.lock();
            m.bgd_table[group].free_inodes_count += 1;
            m.superblock.free_inodes += 1;
        }

        // Enroll BGD page.
        let bgd_page_idx = {
            let bgds_per_block = block_size / BGD_SIZE;
            let bgd_block = 2u64 + (group / bgds_per_block) as u64;
            self.block_to_lba(bgd_block) / 8
        };
        if let Ok(guard) = BlockPageCache::global().read_page(self.device.device_id, bgd_page_idx) {
            tx.enroll_block(self.device.device_id, bgd_page_idx, guard.page_arc());
        }

        // Enroll superblock page (block 1).
        let sb_page_idx = self.block_to_lba(1) / 8;
        if let Ok(guard) = BlockPageCache::global().read_page(self.device.device_id, sb_page_idx) {
            tx.enroll_block(self.device.device_id, sb_page_idx, guard.page_arc());
        }

        Ok(())
    }
}

// ---- Directory entry manipulation --------------------------------------------

impl EfsDriver {
    /// Add a new directory entry to the directory inode `dir_ino`.
    fn add_dir_entry(
        &self,
        dir_ino: u64,
        name: &str,
        entry_ino: u64,
        file_type: u8,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len() as u8;
        let needed = dir_entry_min_size(name_len) as usize;
        let block_size = self.block_size() as usize;

        let dir_inode = self.read_inode(dir_ino)?;
        let dir_size = dir_inode.size as usize;

        // Load all directory data through the block page cache so we see
        // uncommitted dirty pages from prior add/remove_dir_entry calls.
        let mut dir_data = self.read_dir_data_cached(&dir_inode)?;

        // Scan for slack space in existing entries.
        let mut offset = 0usize;
        let mut split_offset: Option<usize> = None;

        while offset + DIR_ENTRY_HEADER_SIZE <= dir_data.len() {
            let hdr: EfsDirEntryHeader = unsafe {
                core::ptr::read_unaligned(dir_data[offset..].as_ptr() as *const EfsDirEntryHeader)
            };
            let rec_len = hdr.rec_len as usize;
            if rec_len < DIR_ENTRY_HEADER_SIZE {
                break;
            }
            let min_len = if hdr.inode != 0 {
                dir_entry_min_size(hdr.name_len) as usize
            } else {
                // Deleted slot — we can reuse it entirely.
                0
            };
            let slack = rec_len.saturating_sub(min_len);
            if slack >= needed {
                split_offset = Some(offset);
                break;
            }
            offset += rec_len;
        }

        if let Some(off) = split_offset {
            // Split the entry at `off`: shrink it to min size and insert ours after.
            let existing_hdr: EfsDirEntryHeader = unsafe {
                core::ptr::read_unaligned(dir_data[off..].as_ptr() as *const EfsDirEntryHeader)
            };
            let rec_len = existing_hdr.rec_len as usize;

            let existing_min = if existing_hdr.inode != 0 {
                dir_entry_min_size(existing_hdr.name_len) as usize
            } else {
                0
            };

            // Write new entry at off + existing_min (or at off for deleted).
            let insert_off = if existing_hdr.inode != 0 {
                // Shrink existing entry's rec_len.
                let new_rec_len = existing_min as u16;
                dir_data[off + 8] = (new_rec_len & 0xFF) as u8;
                dir_data[off + 9] = (new_rec_len >> 8) as u8;
                off + existing_min
            } else {
                off
            };

            let remaining = if existing_hdr.inode != 0 {
                rec_len - existing_min
            } else {
                rec_len
            };

            let new_rec_len = remaining as u16;
            write_dir_entry(
                &mut dir_data[insert_off..insert_off + remaining],
                entry_ino,
                new_rec_len,
                name_len,
                file_type,
                name_bytes,
            );
        } else {
            // Append a new block.
            let new_block_start = dir_size;
            // Grow dir_data by one block.
            dir_data.resize(dir_size + block_size, 0);
            write_dir_entry(
                &mut dir_data[new_block_start..new_block_start + block_size],
                entry_ino,
                block_size as u16,
                name_len,
                file_type,
                name_bytes,
            );

            // Allocate a block and write to it — we also update the inode
            // to know about the new block.
            let new_size = (dir_size + block_size) as u64;
            // Write the new data via ensure_block_for_logical + write_via_extents.
            let dir_inode2 = self.read_inode(dir_ino)?;
            if dir_inode2.flags & INODE_FLAG_INLINE_DATA != 0 {
                // Convert first.
                self.convert_inline_to_extents(dir_ino, &dir_inode2, tx)?;
            }

            let logical_block = (new_block_start / block_size) as u32;
            let phys_block = self.ensure_block_for_logical(dir_ino, logical_block, tx)?;
            self.write_block(
                phys_block,
                &dir_data[new_block_start..new_block_start + block_size],
                tx,
            )?;

            let mut updated = self.read_inode(dir_ino)?;
            updated.size = new_size;
            updated.mtime_sec = current_unix_time();
            updated.checksum = checksum_inode(&updated);
            self.write_inode(dir_ino, &updated, tx)?;
            return Ok(());
        }

        // Write back modified dir_data block by block.
        self.write_dir_blocks(dir_ino, &dir_data, tx)?;

        let mut updated = self.read_inode(dir_ino)?;
        updated.mtime_sec = current_unix_time();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(dir_ino, &updated, tx)
    }

    /// Remove the directory entry with the given name from directory `dir_ino`.
    fn remove_dir_entry(
        &self,
        dir_ino: u64,
        name: &str,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let dir_inode = self.read_inode(dir_ino)?;
        let mut dir_data = self.read_dir_data_cached(&dir_inode)?;

        let mut offset = 0usize;
        let mut prev_end = 0usize;

        while offset + DIR_ENTRY_HEADER_SIZE <= dir_data.len() {
            let hdr: EfsDirEntryHeader = unsafe {
                core::ptr::read_unaligned(dir_data[offset..].as_ptr() as *const EfsDirEntryHeader)
            };
            let rec_len = hdr.rec_len as usize;
            if rec_len < DIR_ENTRY_HEADER_SIZE {
                break;
            }
            if hdr.inode != 0 {
                let name_start = offset + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + hdr.name_len as usize;
                if name_end <= dir_data.len() {
                    if &dir_data[name_start..name_end] == name.as_bytes() {
                        // Mark as deleted.
                        dir_data[offset] = 0;
                        dir_data[offset + 1] = 0;
                        dir_data[offset + 2] = 0;
                        dir_data[offset + 3] = 0;
                        dir_data[offset + 4] = 0;
                        dir_data[offset + 5] = 0;
                        dir_data[offset + 6] = 0;
                        dir_data[offset + 7] = 0;

                        // Try to merge with previous entry (extend its rec_len).
                        if prev_end > 0 {
                            let prev_off = prev_end
                                - find_prev_entry_len(
                                    &dir_data,
                                    prev_end,
                                    self.block_size() as usize,
                                );
                            let prev_hdr: EfsDirEntryHeader = unsafe {
                                core::ptr::read_unaligned(
                                    dir_data[prev_off..].as_ptr() as *const EfsDirEntryHeader
                                )
                            };
                            let new_rec_len = prev_hdr.rec_len as usize + rec_len;
                            dir_data[prev_off + 8] = (new_rec_len & 0xFF) as u8;
                            dir_data[prev_off + 9] = (new_rec_len >> 8) as u8;
                        }

                        self.write_dir_blocks(dir_ino, &dir_data, tx)?;
                        let mut updated = self.read_inode(dir_ino)?;
                        updated.mtime_sec = current_unix_time();
                        updated.checksum = checksum_inode(&updated);
                        return self.write_inode(dir_ino, &updated, tx);
                    }
                }
            }
            prev_end = offset + rec_len;
            offset += rec_len;
        }
        Err(Error::FileNotFound)
    }

    /// Write the in-memory dir_data back to the directory inode's blocks.
    fn write_dir_blocks(
        &self,
        dir_ino: u64,
        dir_data: &[u8],
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let block_size = self.block_size() as usize;
        let dir_inode = self.read_inode(dir_ino)?;

        if dir_inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            // Inline: write directly into inode.
            let mut updated = dir_inode;
            let copy_len = dir_data.len().min(INODE_DATA_AREA_SIZE);
            updated.data_area[..copy_len].copy_from_slice(&dir_data[..copy_len]);
            updated.checksum = checksum_inode(&updated);
            return self.write_inode(dir_ino, &updated, tx);
        }

        // Extent mode: write block by block.
        let mut written = 0usize;
        let mut buf = vec![0u8; block_size];
        while written < dir_data.len() {
            let logical_block = (written / block_size) as u32;
            let phys_block = self.ensure_block_for_logical(dir_ino, logical_block, tx)?;
            let end = (written + block_size).min(dir_data.len());
            buf.fill(0);
            buf[..end - written].copy_from_slice(&dir_data[written..end]);
            self.write_block(phys_block, &buf, tx)?;
            written += block_size;
        }
        Ok(())
    }
}

// ---- Helpers ------------------------------------------------------------------

/// Write a directory entry record into the target byte slice.
fn write_dir_entry(
    buf: &mut [u8],
    inode: u64,
    rec_len: u16,
    name_len: u8,
    file_type: u8,
    name_bytes: &[u8],
) {
    // inode: 8 bytes LE
    buf[0] = (inode & 0xFF) as u8;
    buf[1] = ((inode >> 8) & 0xFF) as u8;
    buf[2] = ((inode >> 16) & 0xFF) as u8;
    buf[3] = ((inode >> 24) & 0xFF) as u8;
    buf[4] = ((inode >> 32) & 0xFF) as u8;
    buf[5] = ((inode >> 40) & 0xFF) as u8;
    buf[6] = ((inode >> 48) & 0xFF) as u8;
    buf[7] = ((inode >> 56) & 0xFF) as u8;
    // rec_len: 2 bytes LE
    buf[8] = (rec_len & 0xFF) as u8;
    buf[9] = (rec_len >> 8) as u8;
    // name_len: 1 byte
    buf[10] = name_len;
    // file_type: 1 byte
    buf[11] = file_type;
    // name bytes
    let name_end = DIR_ENTRY_HEADER_SIZE + name_len as usize;
    if name_end <= buf.len() {
        buf[DIR_ENTRY_HEADER_SIZE..name_end].copy_from_slice(name_bytes);
    }
}

/// Find the length of the previous entry ending at `end_offset`.
fn find_prev_entry_len(data: &[u8], end_offset: usize, block_size: usize) -> usize {
    // Walk from the start of the block containing end_offset.
    let block_start = (end_offset / block_size) * block_size;
    let mut off = block_start;
    let mut prev_len = 0usize;
    while off < end_offset {
        if off + DIR_ENTRY_HEADER_SIZE > data.len() {
            break;
        }
        let rec_len = u16::from_le_bytes([data[off + 8], data[off + 9]]) as usize;
        if rec_len < DIR_ENTRY_HEADER_SIZE {
            break;
        }
        let next = off + rec_len;
        if next == end_offset {
            prev_len = rec_len;
            break;
        }
        if next > end_offset {
            break;
        }
        off = next;
    }
    prev_len
}

fn find_free_bit(bitmap: &[u8], max_bits: usize) -> Option<usize> {
    // Scan 8 bytes (64 bits) at a time for speed.
    let chunks = bitmap.chunks_exact(8);
    let remainder = chunks.remainder();
    for (chunk_idx, chunk) in chunks.enumerate() {
        let val = u64::from_le_bytes(chunk.try_into().unwrap());
        if val == u64::MAX {
            continue;
        }
        let bit = val.trailing_ones() as usize;
        let abs_bit = chunk_idx * 64 + bit;
        return if abs_bit < max_bits {
            Some(abs_bit)
        } else {
            None
        };
    }
    // Handle remaining bytes.
    let base = bitmap.len() - remainder.len();
    for (byte_idx, &byte) in remainder.iter().enumerate() {
        if byte == 0xFF {
            continue;
        }
        let bit = byte.trailing_ones() as usize;
        let abs_bit = (base + byte_idx) * 8 + bit;
        return if abs_bit < max_bits {
            Some(abs_bit)
        } else {
            None
        };
    }
    None
}

fn set_bit(bitmap: &mut [u8], bit: usize) {
    bitmap[bit / 8] |= 1 << (bit % 8);
}

fn clear_bit(bitmap: &mut [u8], bit: usize) {
    bitmap[bit / 8] &= !(1 << (bit % 8));
}

fn current_unix_time() -> u64 {
    // Use the kernel RTC for a reasonable timestamp.
    let rtc = crate::drivers::rtc::read_rtc();
    // Convert to Unix timestamp: compute days from 1970.
    let year = rtc.year as i64;
    let month = rtc.month as i64;
    let day = rtc.day as i64;

    let days = days_since_epoch(year, month, day);
    let secs =
        days as u64 * 86400 + rtc.hour as u64 * 3600 + rtc.minute as u64 * 60 + rtc.second as u64;
    secs
}

fn days_since_epoch(year: i64, month: i64, day: i64) -> i64 {
    // Days from 1970-01-01.
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 12 } else { month };
    let d = day;
    365 * y + y / 4 - y / 100 + y / 400 + (153 * m + 8) / 5 + d - 719469
}

fn unix_to_filetime(secs: u64) -> FileTime {
    let sec = (secs % 60) as u8;
    let total_mins = secs / 60;
    let min = (total_mins % 60) as u8;
    let total_hours = total_mins / 60;
    let hour = (total_hours % 24) as u8;
    let mut days = (total_hours / 24) as u32;

    let mut year = 1970i32;
    loop {
        let days_in_year: u32 = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let months: [u32; 12] = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u8;
    for &m in &months {
        if days < m {
            break;
        }
        days -= m;
        month += 1;
    }
    let day = days as u8 + 1;

    let fat_year = if year >= 1980 {
        (year - 1980) as u16
    } else {
        0
    };
    let date = (fat_year << 9) | ((month as u16) << 5) | (day as u16);
    let time = ((hour as u16) << 11) | ((min as u16) << 5) | ((sec as u16) / 2);
    FileTime {
        date,
        time,
        tenth: 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn inode_to_file(name: String, inode: &EfsInode) -> File {
    let kind = if inode.mode & S_IFMT == S_IFDIR {
        FileKind::Directory
    } else if inode.mode & S_IFMT == S_IFREG {
        FileKind::File
    } else {
        FileKind::Special
    };

    File {
        name,
        kind,
        size: inode.size,
        attrs: FileAttrs {
            readonly: false,
            hidden: false,
            system: false,
            archive: false,
        },
        created: Some(unix_to_filetime(inode.ctime_sec)),
        accessed: Some(unix_to_filetime(inode.atime_sec)),
        modified: Some(unix_to_filetime(inode.mtime_sec)),
    }
}

fn new_inode(mode: u16, flags: u32) -> EfsInode {
    let now = current_unix_time();
    let mut inode = EfsInode {
        mode,
        uid: 0,
        gid: 0,
        link_count: 1,
        size: 0,
        blocks: 0,
        flags,
        reserved1: 0,
        ctime_sec: now,
        ctime_nsec: 0,
        reserved2: 0,
        mtime_sec: now,
        mtime_nsec: 0,
        reserved3: 0,
        atime_sec: now,
        atime_nsec: 0,
        checksum: 0,
        data_area: [0u8; INODE_DATA_AREA_SIZE],
    };
    inode.checksum = checksum_inode(&inode);
    inode
}

// ---- FileSystem trait ---------------------------------------------------------

impl FileSystem for EfsDriver {
    fn list_files(&self, path: &Path) -> Result<Vec<File>, Error> {
        let path = path.normalize();
        let dir_ino = self.resolve_path(&path)?;
        let entries = self.read_dir_entries(dir_ino)?;

        let mut files = Vec::new();
        for (name, ino, _ft) in entries {
            if name == "." || name == ".." {
                continue;
            }
            let inode = self.read_inode(ino)?;
            files.push(inode_to_file(name, &inode));
        }
        Ok(files)
    }

    fn read_bytes(&self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
        let path = path.normalize();
        let (_ino, inode) = self.resolve_path_inode(&path)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }
        self.read_file_data(&inode, offset, count)
    }

    fn write_bytes(&self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
        let path = path.normalize();
        let (ino, inode) = self.resolve_path_inode(&path)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }
        let mut tx = self.journal.begin_tx();
        match self.write_file_data(ino, offset, data, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn create_file(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let name = path.last_component().ok_or(Error::IoError)?.to_string();
        let parent = path.parent().unwrap_or_else(|| Path::parse("/").unwrap());

        let parent_ino = self.resolve_path(&parent)?;
        let parent_inode = self.read_inode(parent_ino)?;
        if parent_inode.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }

        let mut tx = self.journal.begin_tx();
        match self.create_file_inner(parent_ino, &name, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn resolve_inode(&self, path: &Path) -> Result<u64, Error> {
        let path = path.normalize();
        self.resolve_path(&path)
    }

    fn statfs(&self) -> Result<super::StatFs, Error> {
        let m = self.mutable.lock();
        let sb = &m.superblock;
        let mut volume_name = [0u8; 64];
        volume_name.copy_from_slice(&sb.volume_name);
        Ok(super::StatFs {
            fs_type: "efs",
            block_size: 1u64 << sb.block_size_log2,
            total_blocks: sb.total_blocks,
            free_blocks: sb.free_blocks,
            total_inodes: sb.total_inodes,
            free_inodes: sb.free_inodes,
            volume_name,
            version: sb.version,
            block_groups: sb.block_group_count,
        })
    }

    fn read_bytes_ino(&self, ino: u64, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
        let inode = self.read_inode(ino)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }
        self.read_file_data(&inode, offset, count)
    }

    fn write_bytes_ino(&self, ino: u64, offset: usize, data: &[u8]) -> Result<u64, Error> {
        let inode = self.read_inode(ino)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }
        let mut tx = self.journal.begin_tx();
        match self.write_file_data(ino, offset, data, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn file_size_ino(&self, ino: u64) -> Result<u64, Error> {
        let inode = self.read_inode(ino)?;
        Ok(inode.size)
    }

    fn flush_inode(&self, _ino: u64) -> Result<(), Error> {
        // Open an empty TxHandle (no enrollments) so that on drop it merges
        // nothing into the active tx; then force-commit the journal and flush.
        let tx = self.journal.begin_tx();
        drop(tx); // merges empty set — no-op on active tx
        self.journal
            .force_commit_and_wait()
            .map_err(|_| Error::IoError)?;
        self.device.flush()?;
        Ok(())
    }

    fn as_page_cache_ops(&self) -> Option<&dyn crate::fs::page_cache::PageCacheOps> {
        Some(self)
    }

    fn create_dir(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let name = path.last_component().ok_or(Error::IoError)?.to_string();
        let parent = path.parent().unwrap_or_else(|| Path::parse("/").unwrap());

        let parent_ino = self.resolve_path(&parent)?;
        let parent_inode = self.read_inode(parent_ino)?;
        if parent_inode.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }

        let mut tx = self.journal.begin_tx();
        match self.create_dir_inner(parent_ino, &name, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn remove_file(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let name = path.last_component().ok_or(Error::IoError)?.to_string();
        let parent = path.parent().unwrap_or_else(|| Path::parse("/").unwrap());

        let parent_ino = self.resolve_path(&parent)?;
        let file_ino = self.resolve_path(&path)?;
        let file_inode = self.read_inode(file_ino)?;

        if file_inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }

        let mut tx = self.journal.begin_tx();
        match self.remove_file_inner(parent_ino, file_ino, &file_inode, &name, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn remove_dir(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let name = path.last_component().ok_or(Error::IoError)?.to_string();
        let parent = path.parent().unwrap_or_else(|| Path::parse("/").unwrap());

        let parent_ino = self.resolve_path(&parent)?;
        let dir_ino = self.resolve_path(&path)?;
        let dir_inode = self.read_inode(dir_ino)?;

        if dir_inode.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }

        // Verify directory is empty (only "." and "..").
        let entries = self.read_dir_entries(dir_ino)?;
        let non_meta = entries
            .iter()
            .filter(|(n, _, _)| n != "." && n != "..")
            .count();
        if non_meta > 0 {
            return Err(Error::IoError);
        }

        let mut tx = self.journal.begin_tx();
        match self.remove_dir_inner(parent_ino, dir_ino, &dir_inode, &name, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn file_info(&self, path: &Path) -> Result<File, Error> {
        let path = path.normalize();
        let name = if path.is_root() {
            String::from("/")
        } else {
            path.last_component().unwrap_or("/").to_string()
        };
        let (_ino, inode) = self.resolve_path_inode(&path)?;
        Ok(inode_to_file(name, &inode))
    }

    fn flush(&self) -> Result<(), Error> {
        let mut tx = self.journal.begin_tx();
        let result = self.flush_inner(&mut tx);
        if result.is_err() {
            tx.abort();
        }
        result
    }

    fn truncate(&self, path: &Path, size: u64) -> Result<(), Error> {
        let path = path.normalize();
        let (ino, inode) = self.resolve_path_inode(&path)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }

        let mut tx = self.journal.begin_tx();
        match self.truncate_inner(ino, &inode, size, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn rename(&self, old_path: &Path, new_path: &Path) -> Result<(), Error> {
        let old_path = old_path.normalize();
        let new_path = new_path.normalize();

        let old_name = old_path.last_component().ok_or(Error::IoError)?.to_string();
        let new_name = new_path.last_component().ok_or(Error::IoError)?.to_string();

        let old_parent_path = old_path
            .parent()
            .unwrap_or_else(|| Path::parse("/").unwrap());
        let new_parent_path = new_path
            .parent()
            .unwrap_or_else(|| Path::parse("/").unwrap());

        let old_parent_ino = self.resolve_path(&old_parent_path)?;
        let new_parent_ino = self.resolve_path(&new_parent_path)?;
        let target_ino = self.resolve_path(&old_path)?;
        let target_inode = self.read_inode(target_ino)?;

        let mut tx = self.journal.begin_tx();
        match self.rename_inner(
            old_parent_ino,
            new_parent_ino,
            target_ino,
            &target_inode,
            &old_name,
            &new_name,
            &mut tx,
        ) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }
}

// ---- _inner helpers (called by FileSystem trait methods, take &mut TxHandle) ---

impl EfsDriver {
    fn create_file_inner(
        &self,
        parent_ino: u64,
        name: &str,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let new_ino = self.alloc_inode(tx)?;
        let inode = new_inode(S_IFREG | 0o644, INODE_FLAG_INLINE_DATA);
        self.write_inode(new_ino, &inode, tx)?;
        self.add_dir_entry(parent_ino, name, new_ino, FT_REG_FILE, tx)?;
        Ok(())
    }

    fn create_dir_inner(
        &self,
        parent_ino: u64,
        name: &str,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let new_ino = self.alloc_inode(tx)?;

        // Initialize directory inode with "." and ".." entries using a data block.
        let phys_block = self.alloc_block(tx)?;
        let block_size = self.block_size() as usize;
        let mut block_buf = vec![0u8; block_size];

        // "." entry
        write_dir_entry(
            &mut block_buf[..],
            new_ino,
            dir_entry_min_size(1),
            1,
            FT_DIR,
            b".",
        );
        let dot_size = dir_entry_min_size(1) as usize;

        // ".." entry: use the remaining space in the block
        let remaining = (block_size - dot_size) as u16;
        write_dir_entry(
            &mut block_buf[dot_size..],
            parent_ino,
            remaining,
            2,
            FT_DIR,
            b"..",
        );

        self.write_block(phys_block, &block_buf, tx)?;

        // Build extent header for the new dir inode.
        let hdr_size = core::mem::size_of::<EfsExtentHeader>();
        let ext_size = core::mem::size_of::<EfsExtent>();
        let mut data_area = [0u8; INODE_DATA_AREA_SIZE];

        let hdr = EfsExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 1,
            max_entries: efs_common::MAX_INLINE_EXTENTS as u16,
            depth: 0,
            reserved: 0,
        };
        let hdr_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(&hdr as *const EfsExtentHeader as *const u8, hdr_size)
        };
        data_area[..hdr_size].copy_from_slice(hdr_bytes);

        let ext = EfsExtent {
            logical_block: 0,
            length: 1,
            start_hi: (phys_block >> 32) as u16,
            start_lo: phys_block as u32,
        };
        let ext_bytes: &[u8] =
            unsafe { core::slice::from_raw_parts(&ext as *const EfsExtent as *const u8, ext_size) };
        data_area[hdr_size..hdr_size + ext_size].copy_from_slice(ext_bytes);

        let now = current_unix_time();
        let mut inode = EfsInode {
            mode: S_IFDIR | 0o755,
            uid: 0,
            gid: 0,
            link_count: 2, // "." + parent entry
            size: block_size as u64,
            blocks: 1,
            flags: 0,
            reserved1: 0,
            ctime_sec: now,
            ctime_nsec: 0,
            reserved2: 0,
            mtime_sec: now,
            mtime_nsec: 0,
            reserved3: 0,
            atime_sec: now,
            atime_nsec: 0,
            checksum: 0,
            data_area,
        };
        inode.checksum = checksum_inode(&inode);
        self.write_inode(new_ino, &inode, tx)?;

        // Increment parent link_count for the ".." back-reference.
        let mut parent_inode2 = self.read_inode(parent_ino)?;
        parent_inode2.link_count += 1;
        parent_inode2.checksum = checksum_inode(&parent_inode2);
        self.write_inode(parent_ino, &parent_inode2, tx)?;

        // Update BGD used_dirs count.
        let ipg = self.inodes_per_group as usize;
        let group = ((new_ino - 1) as usize) / ipg;
        {
            let mut m = self.mutable.lock();
            if group < m.bgd_table.len() {
                m.bgd_table[group].used_dirs_count += 1;
            }
        }

        self.add_dir_entry(parent_ino, name, new_ino, FT_DIR, tx)
    }

    fn remove_file_inner(
        &self,
        parent_ino: u64,
        file_ino: u64,
        file_inode: &EfsInode,
        name: &str,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        // Free data blocks.
        if file_inode.flags & INODE_FLAG_INLINE_DATA == 0 {
            let hdr: EfsExtentHeader = unsafe {
                core::ptr::read_unaligned(file_inode.data_area.as_ptr() as *const EfsExtentHeader)
            };
            if hdr.magic == EXTENT_MAGIC && hdr.depth == 0 {
                let extents =
                    self.parse_inline_extents(&file_inode.data_area, hdr.entries as usize)?;
                for ext in extents.as_slice() {
                    for i in 0..ext.length as u64 {
                        self.free_block(ext.physical_start() + i, tx)?;
                    }
                }
            }
        }

        self.free_inode(file_ino, tx)?;
        self.remove_dir_entry(parent_ino, name, tx)
    }

    fn remove_dir_inner(
        &self,
        parent_ino: u64,
        dir_ino: u64,
        dir_inode: &EfsInode,
        name: &str,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        // Free data blocks.
        let hdr: EfsExtentHeader = unsafe {
            core::ptr::read_unaligned(dir_inode.data_area.as_ptr() as *const EfsExtentHeader)
        };
        if hdr.magic == EXTENT_MAGIC && hdr.depth == 0 {
            let extents = self.parse_inline_extents(&dir_inode.data_area, hdr.entries as usize)?;
            for ext in extents.as_slice() {
                for i in 0..ext.length as u64 {
                    self.free_block(ext.physical_start() + i, tx)?;
                }
            }
        }

        self.free_inode(dir_ino, tx)?;

        // Decrement parent link_count.
        let mut parent_inode = self.read_inode(parent_ino)?;
        if parent_inode.link_count > 0 {
            parent_inode.link_count -= 1;
        }
        parent_inode.checksum = checksum_inode(&parent_inode);
        self.write_inode(parent_ino, &parent_inode, tx)?;

        // Update BGD used_dirs.
        let ipg = self.inodes_per_group as usize;
        let group = ((dir_ino - 1) as usize) / ipg;
        {
            let mut m = self.mutable.lock();
            if group < m.bgd_table.len() && m.bgd_table[group].used_dirs_count > 0 {
                m.bgd_table[group].used_dirs_count -= 1;
            }
        }

        self.remove_dir_entry(parent_ino, name, tx)
    }

    fn flush_inner(&self, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let m = self.mutable.lock();
        // Write updated superblock to block 1.
        let block_size = self.block_size() as usize;
        let mut sb_block = vec![0u8; block_size];
        let sb_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &m.superblock as *const EfsSuperblock as *const u8,
                core::mem::size_of::<EfsSuperblock>(),
            )
        };
        sb_block[..sb_bytes.len()].copy_from_slice(sb_bytes);
        drop(m);
        self.write_block(1, &sb_block, tx)?;

        let m = self.mutable.lock();
        // Write BGD table starting at block 2.
        let bgd_count = m.bgd_table.len();
        let bgds_per_block = block_size / BGD_SIZE;
        let bgd_blocks = (bgd_count + bgds_per_block - 1) / bgds_per_block;

        let mut bgd_blocks_data: Vec<Vec<u8>> = Vec::with_capacity(bgd_blocks);
        for blk_idx in 0..bgd_blocks {
            let mut blk_buf = vec![0u8; block_size];
            let start = blk_idx * bgds_per_block;
            let end = (start + bgds_per_block).min(bgd_count);
            for (i, bgd) in m.bgd_table[start..end].iter().enumerate() {
                let off = i * BGD_SIZE;
                let bgd_bytes: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        bgd as *const EfsBlockGroupDesc as *const u8,
                        BGD_SIZE,
                    )
                };
                blk_buf[off..off + BGD_SIZE].copy_from_slice(bgd_bytes);
            }
            bgd_blocks_data.push(blk_buf);
        }
        drop(m);

        for (blk_idx, blk_buf) in bgd_blocks_data.iter().enumerate() {
            self.write_block(2 + blk_idx as u64, blk_buf, tx)?;
        }

        self.device.flush()?;
        Ok(())
    }

    fn truncate_inner(
        &self,
        ino: u64,
        inode: &EfsInode,
        size: u64,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let current_size = inode.size;
        if size >= current_size {
            // Growing: just update size (sparse).
            let mut updated = *inode;
            updated.size = size;
            updated.mtime_sec = current_unix_time();
            updated.checksum = checksum_inode(&updated);
            return self.write_inode(ino, &updated, tx);
        }

        // Shrinking: free excess blocks.
        if inode.flags & INODE_FLAG_INLINE_DATA == 0 {
            let block_size = self.block_size() as u64;
            let new_blocks = (size + block_size - 1) / block_size;
            let hdr: EfsExtentHeader = unsafe {
                core::ptr::read_unaligned(inode.data_area.as_ptr() as *const EfsExtentHeader)
            };
            if hdr.magic == EXTENT_MAGIC && hdr.depth == 0 {
                let extents = self.parse_inline_extents(&inode.data_area, hdr.entries as usize)?;
                let mut new_extents = ExtentList {
                    extents: [EfsExtent {
                        logical_block: 0,
                        length: 0,
                        start_hi: 0,
                        start_lo: 0,
                    }; MAX_INLINE_EXTENTS],
                    len: 0,
                };

                for ext in extents.as_slice() {
                    let ext_start = ext.logical_block as u64;
                    let ext_end = ext_start + ext.length as u64;
                    if ext_start >= new_blocks {
                        for i in 0..ext.length as u64 {
                            self.free_block(ext.physical_start() + i, tx)?;
                        }
                    } else if ext_end > new_blocks {
                        let keep = (new_blocks - ext_start) as u16;
                        let free_start = ext.physical_start() + keep as u64;
                        for i in 0..(ext.length - keep) as u64 {
                            self.free_block(free_start + i, tx)?;
                        }
                        let mut trimmed = *ext;
                        trimmed.length = keep;
                        new_extents.push(trimmed);
                    } else {
                        new_extents.push(*ext);
                    }
                }

                // Rebuild extent tree in inode.
                let mut updated = *inode;
                let hdr_size = core::mem::size_of::<EfsExtentHeader>();
                let ext_size = core::mem::size_of::<EfsExtent>();
                let new_hdr = EfsExtentHeader {
                    magic: EXTENT_MAGIC,
                    entries: new_extents.len as u16,
                    max_entries: efs_common::MAX_INLINE_EXTENTS as u16,
                    depth: 0,
                    reserved: 0,
                };
                let hdr_bytes: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        &new_hdr as *const EfsExtentHeader as *const u8,
                        hdr_size,
                    )
                };
                updated.data_area[..hdr_size].copy_from_slice(hdr_bytes);
                for (i, ext) in new_extents.as_slice().iter().enumerate() {
                    let off = hdr_size + i * ext_size;
                    let eb: &[u8] = unsafe {
                        core::slice::from_raw_parts(ext as *const EfsExtent as *const u8, ext_size)
                    };
                    updated.data_area[off..off + ext_size].copy_from_slice(eb);
                }
                updated.blocks = new_extents.as_slice().iter().map(|e| e.length as u64).sum();
                updated.size = size;
                updated.mtime_sec = current_unix_time();
                updated.checksum = checksum_inode(&updated);
                return self.write_inode(ino, &updated, tx);
            }
        }

        // Inline or empty.
        let mut updated = *inode;
        updated.size = size;
        updated.mtime_sec = current_unix_time();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated, tx)
    }

    fn rename_inner(
        &self,
        old_parent_ino: u64,
        new_parent_ino: u64,
        target_ino: u64,
        target_inode: &EfsInode,
        old_name: &str,
        new_name: &str,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let file_type = target_inode.file_type_for_dir_entry();

        self.remove_dir_entry(old_parent_ino, old_name, tx)?;
        self.add_dir_entry(new_parent_ino, new_name, target_ino, file_type, tx)?;

        // If moving a directory, update its ".." entry.
        if target_inode.mode & S_IFMT == S_IFDIR && old_parent_ino != new_parent_ino {
            self.update_dotdot_entry(target_ino, new_parent_ino, tx)?;

            // Adjust link counts of old and new parents.
            let mut old_p = self.read_inode(old_parent_ino)?;
            if old_p.link_count > 0 {
                old_p.link_count -= 1;
            }
            old_p.checksum = checksum_inode(&old_p);
            self.write_inode(old_parent_ino, &old_p, tx)?;

            let mut new_p = self.read_inode(new_parent_ino)?;
            new_p.link_count += 1;
            new_p.checksum = checksum_inode(&new_p);
            self.write_inode(new_parent_ino, &new_p, tx)?;
        }

        Ok(())
    }
}

// ---- PageCacheOps implementation --------------------------------------------

impl PageCacheOps for EfsDriver {
    fn fill_page(&self, ino: u64, page_index: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let inode = self.read_inode(ino)?;
        let file_size = inode.size as usize;
        let offset = page_index as usize * 4096;

        if offset >= file_size {
            buf.fill(0);
            return Ok(0);
        }

        let valid_bytes = (file_size - offset).min(4096);

        if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            let end = (offset + valid_bytes).min(inode.data_area.len());
            buf[..end - offset].copy_from_slice(&inode.data_area[offset..end]);
            if valid_bytes < 4096 {
                buf[valid_bytes..].fill(0);
            }
            return Ok(valid_bytes);
        }

        // Extent-based read: one page == one block (block_size == 4096).
        let hdr: EfsExtentHeader = unsafe {
            core::ptr::read_unaligned(inode.data_area.as_ptr() as *const EfsExtentHeader)
        };
        if hdr.magic != EXTENT_MAGIC || hdr.depth != 0 {
            return Err(Error::Corrupted);
        }

        let extents = self.parse_inline_extents(&inode.data_area, hdr.entries as usize)?;
        let logical_block = page_index as u32;

        let extent = extents
            .as_slice()
            .iter()
            .find(|e| {
                e.logical_block <= logical_block
                    && logical_block < e.logical_block + e.length as u32
            })
            .ok_or(Error::Corrupted)?;

        let phys_block = extent.physical_start() + (logical_block - extent.logical_block) as u64;
        let lba = self.block_to_lba(phys_block);
        let spb = self.sectors_per_block();

        // INVARIANT: file-data page cache does not route through BlockDevice to avoid double-caching. Do not change.
        crate::drivers::ahci::direct::read_sectors(
            self.device.device_id,
            lba,
            spb,
            &mut buf[..spb as usize * 512],
        )?;
        if valid_bytes < 4096 {
            buf[valid_bytes..].fill(0);
        }

        Ok(valid_bytes)
    }

    fn fill_pages_bulk(
        &self,
        ino: u64,
        offset: usize,
        count: usize,
    ) -> Result<alloc::vec::Vec<u8>, Error> {
        let inode = self.read_inode(ino)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }
        self.read_file_data(&inode, offset, count)
    }

    fn flush_page(
        &self,
        ino: u64,
        page_index: u64,
        buf: &[u8],
        _valid_bytes: usize,
    ) -> Result<(), Error> {
        let inode = self.read_inode(ino)?;
        let mut tx = self.journal.begin_tx();

        if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            // Page cache pages are always 4096 bytes, which exceeds the
            // inline data area (176 bytes). Convert to extent mode first.
            if let Err(e) = self.convert_inline_to_extents(ino, &inode, &mut tx) {
                tx.abort();
                return Err(e);
            }
            // Fall through to extent-based write below.
        }

        // Extent-based write.
        let logical_block = page_index as u32;
        let phys_block = match self.ensure_block_for_logical(ino, logical_block, &mut tx) {
            Ok(b) => b,
            Err(e) => {
                tx.abort();
                return Err(e);
            }
        };
        let lba = self.block_to_lba(phys_block);
        let spb = self.sectors_per_block();

        // INVARIANT: file-data page cache does not route through BlockDevice to avoid double-caching. Do not change.
        let needed = spb as usize * 512;
        crate::drivers::ahci::direct::write_sectors(
            self.device.device_id,
            lba,
            &buf[..needed],
            spb,
        )?;

        Ok(())
    }

    fn update_size(&self, ino: u64, new_size: u64) -> Result<(), Error> {
        let inode = self.read_inode(ino)?;
        if new_size <= inode.size {
            return Ok(());
        }
        let mut updated = inode;
        updated.size = new_size;
        updated.mtime_sec = current_unix_time();
        updated.checksum = efs_common::checksum_inode(&updated);
        let mut tx = self.journal.begin_tx();
        match self.write_inode(ino, &updated, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }
}

impl EfsDriver {
    /// Update the ".." directory entry of `dir_ino` to point to `new_parent_ino`.
    fn update_dotdot_entry(
        &self,
        dir_ino: u64,
        new_parent_ino: u64,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let dir_inode = self.read_inode(dir_ino)?;
        let mut dir_data = self.read_dir_data_cached(&dir_inode)?;

        let mut offset = 0usize;
        while offset + DIR_ENTRY_HEADER_SIZE <= dir_data.len() {
            let hdr: EfsDirEntryHeader = unsafe {
                core::ptr::read_unaligned(dir_data[offset..].as_ptr() as *const EfsDirEntryHeader)
            };
            let rec_len = hdr.rec_len as usize;
            if rec_len < DIR_ENTRY_HEADER_SIZE {
                break;
            }
            if hdr.inode != 0 && hdr.name_len == 2 {
                let ns = offset + DIR_ENTRY_HEADER_SIZE;
                if ns + 2 <= dir_data.len() && &dir_data[ns..ns + 2] == b".." {
                    // Patch inode number in place.
                    dir_data[offset] = (new_parent_ino & 0xFF) as u8;
                    dir_data[offset + 1] = ((new_parent_ino >> 8) & 0xFF) as u8;
                    dir_data[offset + 2] = ((new_parent_ino >> 16) & 0xFF) as u8;
                    dir_data[offset + 3] = ((new_parent_ino >> 24) & 0xFF) as u8;
                    dir_data[offset + 4] = ((new_parent_ino >> 32) & 0xFF) as u8;
                    dir_data[offset + 5] = ((new_parent_ino >> 40) & 0xFF) as u8;
                    dir_data[offset + 6] = ((new_parent_ino >> 48) & 0xFF) as u8;
                    dir_data[offset + 7] = ((new_parent_ino >> 56) & 0xFF) as u8;
                    return self.write_dir_blocks(dir_ino, &dir_data, tx);
                }
            }
            offset += rec_len;
        }
        Err(Error::FileNotFound)
    }
}
