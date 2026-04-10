// EFS kernel driver.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use efs_common::{
    DIR_ENTRY_HEADER_SIZE, EFS_ROOT_INO, EXTENT_MAGIC, EfsBlockGroupDesc, EfsDirEntryHeader,
    EfsExtent, EfsExtentHeader, EfsInode, EfsSuperblock, FT_DIR, FT_REG_FILE, INODE_DATA_AREA_SIZE,
    INODE_FLAG_INLINE_DATA, MAX_INLINE_EXTENTS, S_IFDIR, S_IFMT, S_IFREG, checksum_inode,
    dir_entry_min_size,
};

use super::block_device::BlockDevice;
use super::gpt::Partition;
use super::path::Path;
use super::{Error, File, FileAttrs, FileKind, FileSystem, FileTime};

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

/// Maximum number of cached inodes.
const INODE_CACHE_MAX: usize = 64;

pub struct EfsDriver {
    device: BlockDevice,
    partition: Partition,
    superblock: EfsSuperblock,
    bgd_table: Vec<EfsBlockGroupDesc>,
    /// Inode cache: maps inode number -> cached inode.
    inode_cache: BTreeMap<u64, EfsInode>,
    /// Reusable scratch buffer for block reads.
    scratch: Vec<u8>,
}

// ---- Constructor --------------------------------------------------------------

impl EfsDriver {
    pub fn new(partition: Partition) -> Result<Self, Error> {
        let mut device = BlockDevice::new(partition.device_id, 4096);

        // Block 0 = boot/reserved, block 1 = superblock.
        // We don't know block_size yet so read 8 sectors (4 KiB) covering
        // the superblock at the default 4 KiB block boundary.
        let sb_lba = partition.starting_lba + SECTORS_PER_DEFAULT_BLOCK as u64;
        let sb_bytes = device.read_sectors(sb_lba, SECTORS_PER_DEFAULT_BLOCK, vec![])?;

        // SAFETY: EfsSuperblock is repr(C, packed), 256 bytes; the buffer is
        // at least 256 bytes.  We use read_unaligned to avoid UB on packed fields.
        let superblock: EfsSuperblock =
            unsafe { core::ptr::read_unaligned(sb_bytes.as_ptr() as *const EfsSuperblock) };

        superblock.validate().map_err(|_| Error::InvalidFs)?;

        let block_size = 1u64 << superblock.block_size_log2;
        let sectors_per_block = (block_size / 512) as u16;
        let starting_lba = partition.starting_lba;

        // Block 2 = start of BGD table.
        let bgd_lba = starting_lba + 2 * (block_size / 512);
        let bgd_count = superblock.block_group_count as usize;
        // How many sectors do we need for the BGD table?
        let bgd_bytes_needed = bgd_count * BGD_SIZE;
        let bgd_sectors = ((bgd_bytes_needed + 511) / 512).max(sectors_per_block as usize) as u16;
        let bgd_bytes = device.read_sectors(bgd_lba, bgd_sectors, vec![])?;

        let mut bgd_table = Vec::with_capacity(bgd_count);
        for i in 0..bgd_count {
            let offset = i * BGD_SIZE;
            let bgd: EfsBlockGroupDesc = unsafe {
                core::ptr::read_unaligned(bgd_bytes[offset..].as_ptr() as *const EfsBlockGroupDesc)
            };
            bgd_table.push(bgd);
        }

        let bs = 1usize << superblock.block_size_log2;
        Ok(Self {
            device,
            partition,
            superblock,
            bgd_table,
            inode_cache: BTreeMap::new(),
            scratch: vec![0u8; bs],
        })
    }
}

// ---- Low-level block/inode helpers -------------------------------------------

impl EfsDriver {
    fn block_size(&self) -> u64 {
        1u64 << self.superblock.block_size_log2
    }

    fn sectors_per_block(&self) -> u16 {
        (self.block_size() / 512) as u16
    }

    fn block_to_lba(&self, block: u64) -> u64 {
        self.partition.starting_lba + block * (self.block_size() / 512)
    }

    fn read_block(&mut self, block: u64) -> Result<Vec<u8>, Error> {
        let lba = self.block_to_lba(block);
        let spb = self.sectors_per_block();
        // Reuse scratch buffer to avoid allocating a new Vec per read.
        let buf = core::mem::take(&mut self.scratch);
        let result = self.device.read_sectors(lba, spb, buf)?;
        Ok(result)
    }

    /// Return the scratch buffer for reuse after a read_block() call.
    fn recycle(&mut self, buf: Vec<u8>) {
        self.scratch = buf;
    }

    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), Error> {
        let lba = self.block_to_lba(block);
        let spb = self.sectors_per_block();
        // Use a separate buffer for writes to avoid competing with read_block
        // for the scratch buffer (read_block's caller may not have recycled yet).
        let needed = spb as usize * 512;
        let mut buf = vec![0u8; needed];
        let copy_len = data.len().min(needed);
        buf[..copy_len].copy_from_slice(&data[..copy_len]);
        self.device.write_sectors(lba, buf, spb)?;
        Ok(())
    }

    /// Map inode number to (group_index, inode_index_within_group).
    fn inode_location(&self, ino: u64) -> (usize, usize) {
        let ino0 = (ino - 1) as usize; // inodes are 1-based
        let ipg = self.superblock.inodes_per_group as usize;
        (ino0 / ipg, ino0 % ipg)
    }

    fn read_inode(&mut self, ino: u64) -> Result<EfsInode, Error> {
        if let Some(cached) = self.inode_cache.get(&ino) {
            return Ok(*cached);
        }

        let (group, idx) = self.inode_location(ino);
        if group >= self.bgd_table.len() {
            return Err(Error::Corrupted);
        }
        let inode_table_block = self.bgd_table[group].inode_table_block;
        let block_size = self.block_size() as usize;
        let inodes_per_block = block_size / INODE_SIZE;
        let block_offset = idx / inodes_per_block;
        let offset_in_block = (idx % inodes_per_block) * INODE_SIZE;

        let block_data = self.read_block(inode_table_block + block_offset as u64)?;
        let inode: EfsInode = unsafe {
            core::ptr::read_unaligned(block_data[offset_in_block..].as_ptr() as *const EfsInode)
        };
        self.recycle(block_data);

        // Evict oldest entry if cache is full.
        if self.inode_cache.len() >= INODE_CACHE_MAX {
            if let Some(&first_key) = self.inode_cache.keys().next() {
                self.inode_cache.remove(&first_key);
            }
        }
        self.inode_cache.insert(ino, inode);
        Ok(inode)
    }

    fn write_inode(&mut self, ino: u64, inode: &EfsInode) -> Result<(), Error> {
        let (group, idx) = self.inode_location(ino);
        if group >= self.bgd_table.len() {
            return Err(Error::Corrupted);
        }
        let inode_table_block = self.bgd_table[group].inode_table_block;
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
        self.write_block(inode_table_block + block_offset as u64, &block_data)?;
        self.recycle(block_data);
        // Update cache with the new inode data.
        self.inode_cache.insert(ino, *inode);
        Ok(())
    }
}

// ---- File data reading --------------------------------------------------------

impl EfsDriver {
    /// Read up to `count` bytes from a file inode starting at `offset`.
    fn read_file_data(
        &mut self,
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
        &mut self,
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
            // Cap at 128 blocks (512KB) per AHCI command to stay within DMA limits.
            const MAX_BULK_BLOCKS: u32 = 128;
            let blocks_needed =
                ((remaining + offset_in_block + block_size - 1) / block_size) as u32;
            let bulk_blocks = blocks_needed
                .min(blocks_left_in_extent)
                .min(MAX_BULK_BLOCKS);

            let phys_block = extent.physical_start() + block_within_extent as u64;
            let lba = self.block_to_lba(phys_block);
            let total_sectors = (bulk_blocks as u32 * spb as u32) as u16;

            // Read all contiguous blocks in one AHCI command.
            let buf = core::mem::take(&mut self.scratch);
            let bulk_data = self.device.read_sectors(lba, total_sectors, buf)?;

            // Copy the useful portion into result.
            let bulk_bytes = bulk_blocks as usize * block_size;
            let available = bulk_bytes - offset_in_block;
            let copy_len = remaining.min(available);
            result[result_pos..result_pos + copy_len]
                .copy_from_slice(&bulk_data[offset_in_block..offset_in_block + copy_len]);
            self.scratch = bulk_data;

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
    fn read_dir_entries(&mut self, ino: u64) -> Result<Vec<(String, u64, u8)>, Error> {
        let inode = self.read_inode(ino)?;
        if inode.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }
        let dir_data = self.read_file_data(&inode, 0, inode.size as usize)?;
        self.parse_dir_entries_from_bytes(&dir_data)
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
    fn lookup_in_dir(&mut self, dir_ino: u64, name: &str) -> Result<Option<(u64, u8)>, Error> {
        let inode = self.read_inode(dir_ino)?;
        if inode.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }
        let dir_data = self.read_file_data(&inode, 0, inode.size as usize)?;
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
    fn resolve_path(&mut self, path: &Path) -> Result<u64, Error> {
        Ok(self.resolve_path_inode(path)?.0)
    }

    /// Resolve a path to (inode_number, inode), avoiding a redundant read_inode after resolution.
    fn resolve_path_inode(&mut self, path: &Path) -> Result<(u64, EfsInode), Error> {
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
    fn write_file_data(&mut self, ino: u64, byte_offset: usize, data: &[u8]) -> Result<u64, Error> {
        if data.is_empty() {
            return Ok(self.read_inode(ino)?.size);
        }

        let inode = self.read_inode(ino)?;
        let end_offset = byte_offset + data.len();
        let new_size = (end_offset as u64).max(inode.size);

        if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            // Can we still fit inline?
            if end_offset <= INODE_DATA_AREA_SIZE {
                return self.write_inline(ino, &inode, byte_offset, data, new_size);
            }
            // Must convert to extent mode before writing.
            self.convert_inline_to_extents(ino, &inode)?;
        }

        self.write_via_extents(ino, byte_offset, data)?;
        let mut updated = self.read_inode(ino)?;
        if new_size > updated.size {
            updated.size = new_size;
        }
        updated.mtime_sec = current_unix_time();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated)?;
        Ok(updated.size)
    }

    fn write_inline(
        &mut self,
        ino: u64,
        inode: &EfsInode,
        offset: usize,
        data: &[u8],
        new_size: u64,
    ) -> Result<u64, Error> {
        let mut updated = *inode;
        updated.data_area[offset..offset + data.len()].copy_from_slice(data);
        updated.size = new_size;
        updated.mtime_sec = current_unix_time();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated)?;
        Ok(new_size)
    }

    fn convert_inline_to_extents(&mut self, ino: u64, inode: &EfsInode) -> Result<(), Error> {
        let inline_data = inode.data_area[..inode.size as usize].to_vec();
        let block_size = self.block_size() as usize;

        // Allocate one block for the data.
        let phys_block = self.alloc_block()?;
        let mut block_buf = vec![0u8; block_size];
        block_buf[..inline_data.len()].copy_from_slice(&inline_data);
        self.write_block(phys_block, &block_buf)?;

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
        self.write_inode(ino, &updated)
    }

    fn write_via_extents(
        &mut self,
        ino: u64,
        byte_offset: usize,
        data: &[u8],
    ) -> Result<(), Error> {
        let block_size = self.block_size() as usize;
        let mut written = 0usize;

        while written < data.len() {
            let cur_byte = byte_offset + written;
            let logical_block = (cur_byte / block_size) as u32;
            let offset_in_block = cur_byte % block_size;
            let copy_len = (data.len() - written).min(block_size - offset_in_block);

            let phys_block = self.ensure_block_for_logical(ino, logical_block)?;

            let mut block_data = self.read_block(phys_block)?;
            block_data[offset_in_block..offset_in_block + copy_len]
                .copy_from_slice(&data[written..written + copy_len]);
            self.write_block(phys_block, &block_data)?;

            written += copy_len;
        }
        Ok(())
    }

    /// Return the physical block for the given logical block, allocating if needed.
    fn ensure_block_for_logical(&mut self, ino: u64, logical_block: u32) -> Result<u64, Error> {
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
        let phys_block = self.alloc_block()?;
        // Zero it.
        let block_size = self.block_size() as usize;
        self.write_block(phys_block, &vec![0u8; block_size])?;

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
        self.write_inode(ino, &updated)?;

        Ok(phys_block)
    }
}

// ---- Bitmap operations --------------------------------------------------------

impl EfsDriver {
    /// Allocate a free block and return its absolute block number.
    fn alloc_block(&mut self) -> Result<u64, Error> {
        let block_size = self.block_size() as usize;
        let blocks_per_group = self.superblock.blocks_per_group as usize;

        for g in 0..self.bgd_table.len() {
            if self.bgd_table[g].free_blocks_count == 0 {
                continue;
            }
            let bitmap_block = self.bgd_table[g].block_bitmap_block;
            let mut bitmap = self.read_block(bitmap_block)?;

            let bits_to_check = blocks_per_group.min(block_size * 8);
            if let Some(bit) = find_free_bit(&bitmap, bits_to_check) {
                set_bit(&mut bitmap, bit);
                self.write_block(bitmap_block, &bitmap)?;
                self.recycle(bitmap);

                self.bgd_table[g].free_blocks_count -= 1;
                self.superblock.free_blocks -= 1;

                let abs_block = g as u64 * blocks_per_group as u64 + bit as u64;
                return Ok(abs_block);
            }
            self.recycle(bitmap);
        }
        Err(Error::IoError)
    }

    /// Free a block (by absolute block number).
    fn free_block(&mut self, block: u64) -> Result<(), Error> {
        let blocks_per_group = self.superblock.blocks_per_group as u64;
        let group = (block / blocks_per_group) as usize;
        let bit = (block % blocks_per_group) as usize;

        if group >= self.bgd_table.len() {
            return Err(Error::Corrupted);
        }

        let bitmap_block = self.bgd_table[group].block_bitmap_block;
        let mut bitmap = self.read_block(bitmap_block)?;
        clear_bit(&mut bitmap, bit);
        self.write_block(bitmap_block, &bitmap)?;
        self.recycle(bitmap);

        self.bgd_table[group].free_blocks_count += 1;
        self.superblock.free_blocks += 1;
        Ok(())
    }

    /// Allocate a free inode and return its inode number (1-based).
    fn alloc_inode(&mut self) -> Result<u64, Error> {
        let block_size = self.block_size() as usize;
        let inodes_per_group = self.superblock.inodes_per_group as usize;

        for g in 0..self.bgd_table.len() {
            if self.bgd_table[g].free_inodes_count == 0 {
                continue;
            }
            let bitmap_block = self.bgd_table[g].inode_bitmap_block;
            let mut bitmap = self.read_block(bitmap_block)?;

            let bits_to_check = inodes_per_group.min(block_size * 8);
            if let Some(bit) = find_free_bit(&bitmap, bits_to_check) {
                set_bit(&mut bitmap, bit);
                self.write_block(bitmap_block, &bitmap)?;
                self.recycle(bitmap);

                self.bgd_table[g].free_inodes_count -= 1;
                self.superblock.free_inodes -= 1;

                let ino = g as u64 * inodes_per_group as u64 + bit as u64 + 1;
                return Ok(ino);
            }
            self.recycle(bitmap);
        }
        Err(Error::IoError)
    }

    /// Free an inode.
    fn free_inode(&mut self, ino: u64) -> Result<(), Error> {
        let inodes_per_group = self.superblock.inodes_per_group as usize;
        let ino0 = (ino - 1) as usize;
        let group = ino0 / inodes_per_group;
        let bit = ino0 % inodes_per_group;

        if group >= self.bgd_table.len() {
            return Err(Error::Corrupted);
        }

        let bitmap_block = self.bgd_table[group].inode_bitmap_block;
        let mut bitmap = self.read_block(bitmap_block)?;
        clear_bit(&mut bitmap, bit);
        self.write_block(bitmap_block, &bitmap)?;
        self.recycle(bitmap);

        self.bgd_table[group].free_inodes_count += 1;
        self.superblock.free_inodes += 1;
        Ok(())
    }
}

// ---- Directory entry manipulation --------------------------------------------

impl EfsDriver {
    /// Add a new directory entry to the directory inode `dir_ino`.
    fn add_dir_entry(
        &mut self,
        dir_ino: u64,
        name: &str,
        entry_ino: u64,
        file_type: u8,
    ) -> Result<(), Error> {
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len() as u8;
        let needed = dir_entry_min_size(name_len) as usize;
        let block_size = self.block_size() as usize;

        let dir_inode = self.read_inode(dir_ino)?;
        let dir_size = dir_inode.size as usize;

        // Load all directory data.
        let mut dir_data = self.read_file_data(&dir_inode, 0, dir_size)?;

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
                self.convert_inline_to_extents(dir_ino, &dir_inode2)?;
            }

            let logical_block = (new_block_start / block_size) as u32;
            let phys_block = self.ensure_block_for_logical(dir_ino, logical_block)?;
            self.write_block(
                phys_block,
                &dir_data[new_block_start..new_block_start + block_size],
            )?;

            let mut updated = self.read_inode(dir_ino)?;
            updated.size = new_size;
            updated.mtime_sec = current_unix_time();
            updated.checksum = checksum_inode(&updated);
            self.write_inode(dir_ino, &updated)?;
            return Ok(());
        }

        // Write back modified dir_data block by block.
        self.write_dir_blocks(dir_ino, &dir_data)?;

        let mut updated = self.read_inode(dir_ino)?;
        updated.mtime_sec = current_unix_time();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(dir_ino, &updated)
    }

    /// Remove the directory entry with the given name from directory `dir_ino`.
    fn remove_dir_entry(&mut self, dir_ino: u64, name: &str) -> Result<(), Error> {
        let dir_inode = self.read_inode(dir_ino)?;
        let dir_size = dir_inode.size as usize;
        let mut dir_data = self.read_file_data(&dir_inode, 0, dir_size)?;

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

                        self.write_dir_blocks(dir_ino, &dir_data)?;
                        let mut updated = self.read_inode(dir_ino)?;
                        updated.mtime_sec = current_unix_time();
                        updated.checksum = checksum_inode(&updated);
                        return self.write_inode(dir_ino, &updated);
                    }
                }
            }
            prev_end = offset + rec_len;
            offset += rec_len;
        }
        Err(Error::FileNotFound)
    }

    /// Write the in-memory dir_data back to the directory inode's blocks.
    fn write_dir_blocks(&mut self, dir_ino: u64, dir_data: &[u8]) -> Result<(), Error> {
        let block_size = self.block_size() as usize;
        let dir_inode = self.read_inode(dir_ino)?;

        if dir_inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            // Inline: write directly into inode.
            let mut updated = dir_inode;
            let copy_len = dir_data.len().min(INODE_DATA_AREA_SIZE);
            updated.data_area[..copy_len].copy_from_slice(&dir_data[..copy_len]);
            updated.checksum = checksum_inode(&updated);
            return self.write_inode(dir_ino, &updated);
        }

        // Extent mode: write block by block.
        let mut written = 0usize;
        let mut buf = vec![0u8; block_size];
        while written < dir_data.len() {
            let logical_block = (written / block_size) as u32;
            let phys_block = self.ensure_block_for_logical(dir_ino, logical_block)?;
            let end = (written + block_size).min(dir_data.len());
            buf.fill(0);
            buf[..end - written].copy_from_slice(&dir_data[written..end]);
            self.write_block(phys_block, &buf)?;
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
    fn list_files(&mut self, path: &Path) -> Result<Vec<File>, Error> {
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

    fn read_bytes(&mut self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
        let path = path.normalize();
        let (_ino, inode) = self.resolve_path_inode(&path)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }
        self.read_file_data(&inode, offset, count)
    }

    fn write_bytes(&mut self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
        let path = path.normalize();
        let (ino, inode) = self.resolve_path_inode(&path)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }
        self.write_file_data(ino, offset, data)
    }

    fn create_file(&mut self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let name = path.last_component().ok_or(Error::IoError)?.to_string();
        let parent = path.parent().unwrap_or_else(|| Path::parse("/").unwrap());

        let parent_ino = self.resolve_path(&parent)?;
        let parent_inode = self.read_inode(parent_ino)?;
        if parent_inode.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }

        let new_ino = self.alloc_inode()?;
        let inode = new_inode(S_IFREG | 0o644, INODE_FLAG_INLINE_DATA);
        self.write_inode(new_ino, &inode)?;

        // Update group dir count if needed (file, not dir — skip used_dirs).
        self.add_dir_entry(parent_ino, &name, new_ino, FT_REG_FILE)?;
        Ok(())
    }

    fn create_dir(&mut self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let name = path.last_component().ok_or(Error::IoError)?.to_string();
        let parent = path.parent().unwrap_or_else(|| Path::parse("/").unwrap());

        let parent_ino = self.resolve_path(&parent)?;
        let parent_inode = self.read_inode(parent_ino)?;
        if parent_inode.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }

        let new_ino = self.alloc_inode()?;

        // Initialize directory inode with "." and ".." entries using a data block.
        let phys_block = self.alloc_block()?;
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

        self.write_block(phys_block, &block_buf)?;

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
        self.write_inode(new_ino, &inode)?;

        // Increment parent link_count for the ".." back-reference.
        let mut parent_inode2 = self.read_inode(parent_ino)?;
        parent_inode2.link_count += 1;
        parent_inode2.checksum = checksum_inode(&parent_inode2);
        self.write_inode(parent_ino, &parent_inode2)?;

        // Update BGD used_dirs count.
        let ipg = self.superblock.inodes_per_group as usize;
        let group = ((new_ino - 1) as usize) / ipg;
        if group < self.bgd_table.len() {
            self.bgd_table[group].used_dirs_count += 1;
        }

        self.add_dir_entry(parent_ino, &name, new_ino, FT_DIR)
    }

    fn remove_file(&mut self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let name = path.last_component().ok_or(Error::IoError)?.to_string();
        let parent = path.parent().unwrap_or_else(|| Path::parse("/").unwrap());

        let parent_ino = self.resolve_path(&parent)?;
        let file_ino = self.resolve_path(&path)?;
        let file_inode = self.read_inode(file_ino)?;

        if file_inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }

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
                        self.free_block(ext.physical_start() + i)?;
                    }
                }
            }
        }

        self.free_inode(file_ino)?;
        self.remove_dir_entry(parent_ino, &name)
    }

    fn remove_dir(&mut self, path: &Path) -> Result<(), Error> {
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

        // Free data blocks.
        let hdr: EfsExtentHeader = unsafe {
            core::ptr::read_unaligned(dir_inode.data_area.as_ptr() as *const EfsExtentHeader)
        };
        if hdr.magic == EXTENT_MAGIC && hdr.depth == 0 {
            let extents = self.parse_inline_extents(&dir_inode.data_area, hdr.entries as usize)?;
            for ext in extents.as_slice() {
                for i in 0..ext.length as u64 {
                    self.free_block(ext.physical_start() + i)?;
                }
            }
        }

        self.free_inode(dir_ino)?;

        // Decrement parent link_count.
        let mut parent_inode = self.read_inode(parent_ino)?;
        if parent_inode.link_count > 0 {
            parent_inode.link_count -= 1;
        }
        parent_inode.checksum = checksum_inode(&parent_inode);
        self.write_inode(parent_ino, &parent_inode)?;

        // Update BGD used_dirs.
        let ipg = self.superblock.inodes_per_group as usize;
        let group = ((dir_ino - 1) as usize) / ipg;
        if group < self.bgd_table.len() && self.bgd_table[group].used_dirs_count > 0 {
            self.bgd_table[group].used_dirs_count -= 1;
        }

        self.remove_dir_entry(parent_ino, &name)
    }

    fn file_info(&mut self, path: &Path) -> Result<File, Error> {
        let path = path.normalize();
        let name = if path.is_root() {
            String::from("/")
        } else {
            path.last_component().unwrap_or("/").to_string()
        };
        let (_ino, inode) = self.resolve_path_inode(&path)?;
        Ok(inode_to_file(name, &inode))
    }

    fn flush(&mut self) -> Result<(), Error> {
        // Write updated superblock to block 1.
        let block_size = self.block_size() as usize;
        let mut sb_block = vec![0u8; block_size];
        let sb_bytes: &[u8] = unsafe {
            core::slice::from_raw_parts(
                &self.superblock as *const EfsSuperblock as *const u8,
                core::mem::size_of::<EfsSuperblock>(),
            )
        };
        sb_block[..sb_bytes.len()].copy_from_slice(sb_bytes);
        self.write_block(1, &sb_block)?;

        // Write BGD table starting at block 2.
        let bgd_count = self.bgd_table.len();
        let bgds_per_block = block_size / BGD_SIZE;
        let bgd_blocks = (bgd_count + bgds_per_block - 1) / bgds_per_block;

        for blk_idx in 0..bgd_blocks {
            let mut blk_buf = vec![0u8; block_size];
            let start = blk_idx * bgds_per_block;
            let end = (start + bgds_per_block).min(bgd_count);
            for (i, bgd) in self.bgd_table[start..end].iter().enumerate() {
                let off = i * BGD_SIZE;
                let bgd_bytes: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        bgd as *const EfsBlockGroupDesc as *const u8,
                        BGD_SIZE,
                    )
                };
                blk_buf[off..off + BGD_SIZE].copy_from_slice(bgd_bytes);
            }
            self.write_block(2 + blk_idx as u64, &blk_buf)?;
        }

        self.device.flush()?;
        Ok(())
    }

    fn truncate(&mut self, path: &Path, size: u64) -> Result<(), Error> {
        let path = path.normalize();
        let (ino, inode) = self.resolve_path_inode(&path)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }

        let current_size = inode.size;
        if size >= current_size {
            // Growing: just update size (sparse).
            let mut updated = inode;
            updated.size = size;
            updated.mtime_sec = current_unix_time();
            updated.checksum = checksum_inode(&updated);
            return self.write_inode(ino, &updated);
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
                            self.free_block(ext.physical_start() + i)?;
                        }
                    } else if ext_end > new_blocks {
                        let keep = (new_blocks - ext_start) as u16;
                        let free_start = ext.physical_start() + keep as u64;
                        for i in 0..(ext.length - keep) as u64 {
                            self.free_block(free_start + i)?;
                        }
                        let mut trimmed = *ext;
                        trimmed.length = keep;
                        new_extents.push(trimmed);
                    } else {
                        new_extents.push(*ext);
                    }
                }

                // Rebuild extent tree in inode.
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
                return self.write_inode(ino, &updated);
            }
        }

        // Inline or empty.
        let mut updated = inode;
        updated.size = size;
        updated.mtime_sec = current_unix_time();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated)
    }

    fn rename(&mut self, old_path: &Path, new_path: &Path) -> Result<(), Error> {
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

        let file_type = target_inode.file_type_for_dir_entry();

        self.remove_dir_entry(old_parent_ino, &old_name)?;
        self.add_dir_entry(new_parent_ino, &new_name, target_ino, file_type)?;

        // If moving a directory, update its ".." entry.
        if target_inode.mode & S_IFMT == S_IFDIR && old_parent_ino != new_parent_ino {
            self.update_dotdot_entry(target_ino, new_parent_ino)?;

            // Adjust link counts of old and new parents.
            let mut old_p = self.read_inode(old_parent_ino)?;
            if old_p.link_count > 0 {
                old_p.link_count -= 1;
            }
            old_p.checksum = checksum_inode(&old_p);
            self.write_inode(old_parent_ino, &old_p)?;

            let mut new_p = self.read_inode(new_parent_ino)?;
            new_p.link_count += 1;
            new_p.checksum = checksum_inode(&new_p);
            self.write_inode(new_parent_ino, &new_p)?;
        }

        Ok(())
    }

    fn statfs(&mut self) -> Result<super::StatFs, Error> {
        let sb = &self.superblock;
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
}

impl EfsDriver {
    /// Update the ".." directory entry of `dir_ino` to point to `new_parent_ino`.
    fn update_dotdot_entry(&mut self, dir_ino: u64, new_parent_ino: u64) -> Result<(), Error> {
        let dir_inode = self.read_inode(dir_ino)?;
        let dir_size = dir_inode.size as usize;
        let mut dir_data = self.read_file_data(&dir_inode, 0, dir_size)?;

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
                    return self.write_dir_blocks(dir_ino, &dir_data);
                }
            }
            offset += rec_len;
        }
        Err(Error::FileNotFound)
    }
}
