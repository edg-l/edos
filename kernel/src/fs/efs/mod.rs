// EFS kernel driver.

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use efs_common::{
    DIR_ENTRY_HEADER_SIZE, EFS_ROOT_INO, EXTENT_MAGIC, EfsBlockGroupDesc, EfsDirEntryHeader,
    EfsExtent, EfsExtentHeader, EfsInode, EfsSuperblock, FT_DIR, FT_FIFO, FT_REG_FILE, FT_SYMLINK,
    INCOMPAT_JOURNAL, INODE_DATA_AREA_SIZE, INODE_FLAG_INLINE_DATA, JOURNAL_MAGIC,
    JournalSuperblock, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, checksum_inode,
    checksum_superblock, dir_entry_min_size, journal_sb_checksum,
};

mod extents;
use extents::{BlockRun, ExtentMap};

use super::block_device::BlockDevice;
use super::block_page_cache::BlockPageCache;
use super::gpt::Partition;
use super::journal::{Journal, tx::TxHandle};
use super::page_cache::{CachedPage, PageCacheOps};
use super::page_fill::{PrefetchPlan, PrefetchRun};
use crate::drivers::block_io::{self, BlockBuffer, BlockError, BlockIoHandle, WriteFlags};

/// Blocks one AHCI command may carry, from the 248-entry PRDT (992 KiB).
/// A device whose maximum transfer is smaller splits the request itself --
/// NVMe chops at MDTS in `drivers::nvme::namespace` -- so this stays the
/// fs-layer batch size rather than the minimum any device can accept.
const MAX_RUN_BLOCKS: usize = 248;

fn block_read(device_id: u64, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    let dev = block_io::lookup(device_id).ok_or(BlockError::DeviceGone)?;
    block_io::read_blocking(&dev, lba, buf)?;
    Ok(())
}

fn block_write(device_id: u64, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let dev = block_io::lookup(device_id).ok_or(BlockError::DeviceGone)?;
    block_io::write_blocking(&dev, lba, buf, WriteFlags::NONE)?;
    Ok(())
}

/// Issue a sector-level write and return its handle without waiting, so the
/// caller can keep further commands outstanding behind it. The op co-owns
/// `buf` via the `Arc` clone inside `BlockBuffer::owned_vec`.
fn submit_block_write(
    device_id: u64,
    lba: u64,
    sectors: u16,
    buf: Arc<Vec<u8>>,
) -> Result<Arc<BlockIoHandle>, BlockError> {
    let dev = block_io::lookup(device_id).ok_or(BlockError::DeviceGone)?;
    let handle = dev.submit_write(
        lba,
        sectors as u32,
        BlockBuffer::owned_vec(buf),
        WriteFlags::NONE,
    )?;
    Ok(handle)
}

/// One `submit_block_write` that has not been waited on yet, kept with what it
/// would take to issue again: recovering a hung controller fails whatever was
/// in flight, and those bytes are a file's data.
struct InflightWrite {
    handle: Arc<BlockIoHandle>,
    staging: Arc<Vec<u8>>,
    lba: u64,
    sectors: u16,
    first_page: u64,
    pages: u64,
}

/// Wait for one outstanding write, then drop the block cache's copy of the
/// range it overwrote behind the cache's back. The invalidation happens on
/// failure too: a command that reported an error may still have landed in
/// part, so a cached page for that range cannot be trusted either way.
fn reap_write(device_id: u64, mut write: InflightWrite) -> Result<(), BlockError> {
    let started = crate::timer::Instant::now();
    let result: Result<(), BlockError> = loop {
        match write.handle.wait() {
            Ok(()) => break Ok(()),
            Err(e) if block_io::retry_after(e, started) => {
                match submit_block_write(device_id, write.lba, write.sectors, write.staging.clone())
                {
                    Ok(handle) => write.handle = handle,
                    Err(e) => break Err(e),
                }
            }
            Err(e) => break Err(e),
        }
    };
    // The device is only done reading the staging buffer now.
    drop(write.staging);
    BlockPageCache::global().invalidate_pages(device_id, write.first_page, write.pages);
    result?;
    Ok(())
}

fn block_write_fua(device_id: u64, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let dev = block_io::lookup(device_id).ok_or(BlockError::DeviceGone)?;
    block_io::write_blocking(&dev, lba, buf, WriteFlags::FUA)?;
    Ok(())
}
use super::path::Path;
use super::{
    DateTime, Error, File, FileAttrs, FileKind, FileSystem, FileTime, LinkEscape, LinkMode,
    splice_symlink,
};
use crate::{
    debug::lock_order::{RANK_EFS_BITMAP, RANK_EFS_INODE_RMW, RANK_EFS_MUTABLE, RANK_EFS_ORPHAN},
    log, ranked_lock,
    thread::mutex::BlockingMutex,
};

// ---- Constants ----------------------------------------------------------------

/// Whether a freshly allocated block has to be zeroed on disk before it is
/// mapped into a file.
///
/// Zeroing is a journalled write of the block, so the journal carries a copy of
/// that block's home location full of zeros. File data bypasses the block page
/// cache and goes straight to the device, which means the zeros are written to
/// the home block *after* the data whenever the copy reaches it: a concurrent
/// `flush_dirty_once` from another thread's fsync, or a replay of the ring after
/// a reboot that found the transaction committed but not checkpointed. A caller
/// that overwrites every byte of the block must therefore not stage them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NewBlock {
    /// The caller may read the block back, or write only part of it, so it must
    /// start as zeros rather than whatever its previous life left behind.
    Zeroed,
    /// The caller writes the whole block before it returns.
    Overwritten,
}

/// Where a path walk stopped.
// The large variant is the common one and the value is consumed immediately by
// the caller, so boxing it would add an allocation to every successful walk.
#[allow(clippy::large_enum_variant)]
enum Walk {
    Node((u64, EfsInode)),
    /// A symbolic link named a path outside this filesystem.
    Escaped(LinkEscape),
}

/// Number of 512-byte sectors in 4 KiB (one default block).
const SECTORS_PER_DEFAULT_BLOCK: u16 = 8;

/// Size of one block group descriptor on disk.
const BGD_SIZE: usize = core::mem::size_of::<EfsBlockGroupDesc>();

/// Size of one inode on disk.
const INODE_SIZE: usize = core::mem::size_of::<EfsInode>();

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
    /// Serializes bitmap-based allocation (`alloc_inode` / `alloc_block`) so
    /// that `mutable` can be released across BPC I/O without two concurrent
    /// allocators picking the same bit. Rank 105 (above `DIRTY_INODES` 100,
    /// below `BPC.shard` 110).
    /// Guards every read-modify-write of an allocation bitmap. See
    /// `RANK_EFS_BITMAP`.
    bitmap_mutex: BlockingMutex<()>,
    /// Serializes read-modify-write of an inode. See `RANK_EFS_INODE_RMW`.
    inode_rmw: BlockingMutex<()>,
    /// In-memory mirror of the on-disk orphan chain: inode number → the inode
    /// that points at it, where 0 means the superblock's `last_orphan` does.
    ///
    /// The chain is singly linked on disk, so unlinking an inode from the middle
    /// of it needs its predecessor, and walking from the head to find one is
    /// O(chain) per eviction over a chain that reaches into the hundreds. This is
    /// the same reason ext4 keeps `i_orphan` in memory beside the on-disk chain.
    /// It starts empty at every mount, because mount finishes and clears whatever
    /// the previous one left behind.
    orphan_prev: BlockingMutex<BTreeMap<u64, u64>>,
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
        let bgd_pages = bgd_bytes_needed.div_ceil(4096).max(1);
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
        // The superblock's head is deliberately not read here. It is written
        // only from `advance_tail`, so it lags every transaction committed
        // since the last checkpoint; the tail anchors the scan and replay
        // reports where the live region actually ended.
        let jsb_tail_seq = jsb.tail_seq;
        let jsb_tail_block = jsb.tail_block;

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

        // Replay committed-but-uncheckpointed journal transactions before
        // constructing the live Journal (which opens a fresh active tx).
        let replay_result = crate::fs::journal::replay::replay(
            partition.device_id,
            j_first_block,
            jsb_block_count,
            starting_lba,
            jsb_tail_seq,
            jsb_tail_block,
        )?;

        // Restart the journal where replay found the live region to end, not
        // where the superblock's head claimed it was. That head is written
        // only from `advance_tail`, so after a crash it names a position older
        // than what replay just applied: restarting there would reissue
        // sequence numbers already on disk and overwrite live ring blocks.
        let post_replay_seq = replay_result.next_seq;
        let post_replay_block = replay_result.next_block;

        if replay_result.txs_applied > 0 {
            let updated_jsb = JournalSuperblock {
                magic: JOURNAL_MAGIC,
                version: 1,
                block_count: jsb_block_count,
                block_size: 4096,
                tail_seq: post_replay_seq,
                head_seq: post_replay_seq,
                tail_block: post_replay_block,
                head_block: post_replay_block,
                crc32: 0,
                reserved: [0u8; 12],
            };
            let crc = journal_sb_checksum(&updated_jsb);
            let updated_jsb = JournalSuperblock {
                crc32: crc,
                ..updated_jsb
            };
            let jsb_lba = j_lba;
            let mut jsb_block = vec![0u8; 4096];
            let jsb_bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(
                    &updated_jsb as *const JournalSuperblock as *const u8,
                    core::mem::size_of::<JournalSuperblock>(),
                )
            };
            jsb_block[..jsb_bytes.len()].copy_from_slice(jsb_bytes);
            block_write_fua(partition.device_id, jsb_lba, &jsb_block)?;
        }

        // j_first_block is already a partition-relative EFS block number.
        let journal = Journal::new(
            partition.device_id,
            starting_lba,
            j_first_block,
            jsb_block_count,
            post_replay_seq,
            post_replay_seq,
            post_replay_block,
        );

        // Register the journal with the block page cache so writeback can
        // gate flushes on commit state.
        BlockPageCache::global().register_device(partition.device_id, journal.clone());

        let driver = Self {
            device,
            partition,
            block_size_log2,
            inodes_per_group,
            journal,
            bitmap_mutex: BlockingMutex::new(()),
            inode_rmw: BlockingMutex::new(()),
            orphan_prev: BlockingMutex::new(BTreeMap::new()),
            mutable: BlockingMutex::new(EfsMutableState {
                superblock,
                bgd_table,
            }),
        };

        // Finish the deletions the last mount was interrupted in the middle of.
        // After replay, because replay is what restores the chain the committed
        // transactions describe. A failure here is reported rather than fatal:
        // the filesystem is consistent either way, it just still holds inodes
        // that only `efs-fsck --repair` would reclaim.
        if let Err(e) = driver.process_orphan_list() {
            log!(
                "efs: orphan list not fully processed ({:?}); run efs-fsck",
                e
            );
        }

        Ok(driver)
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

    /// Write one metadata block and enrol it in `tx`.
    ///
    /// The page the write went into is what gets enrolled. Looking the key up
    /// again instead would enrol whatever page a second lookup returns, which is
    /// not always the page just written: under cache pressure the write can land
    /// on a detached page, and the lookup then reads the block back off the disk
    /// and enrols *that*, so the journal records the bytes on the platter rather
    /// than the bytes being written.
    fn write_block(&self, block: u64, data: &[u8], tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let lba = self.block_to_lba(block);
        let page_idx = lba / 8;
        let mut buf = [0u8; 4096];
        let n = data.len().min(4096);
        buf[..n].copy_from_slice(&data[..n]);
        let guard = self.device.write_page(page_idx, &buf)?;
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
            let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
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
            let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
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

/// Commands kept in flight by one bulk file-data read. The port has 32 NCQ
/// slots and other threads share them.
const MAX_INFLIGHT_READS: usize = 16;
/// Staging bytes held by those commands at once, so a very large read costs a
/// bounded amount of memory beyond its own result buffer.
const MAX_INFLIGHT_BYTES: usize = 2 * 1024 * 1024;
/// Commands one readahead window may queue. Half the port's NCQ slots, because
/// a prefetch is speculative and must leave room for the reads a thread is
/// actually waiting on. A window needing more runs than this is prefetched as
/// far as the budget reaches and no further.
const MAX_PREFETCH_RUNS: usize = 16;

/// One device read planned by `read_via_extents`: a physically contiguous run,
/// and where in the caller's buffer its bytes land.
struct ExtentRead {
    lba: u64,
    sectors: u16,
    /// Offset of the run's first wanted byte in the result buffer.
    dest: usize,
    /// Offset of that byte inside the run's first block.
    skew: usize,
    len: usize,
}

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
            // Data lives directly in data_area. Invariant: for inline-mode
            // inodes, `size <= INODE_DATA_AREA_SIZE` (176 bytes) is enforced
            // by `update_size`, which converts inline->extents before
            // stamping any size that would overflow the inline area.
            let end = (offset + to_read).min(inode.data_area.len());
            return Ok(inode.data_area[offset..end].to_vec());
        }

        self.read_via_extents(inode, offset, to_read)
    }

    fn read_via_extents(
        &self,
        inode: &EfsInode,
        byte_offset: usize,
        count: usize,
    ) -> Result<Vec<u8>, Error> {
        let extents = self.load_extent_map(inode)?;
        let block_size = self.block_size() as usize;
        let spb = self.sectors_per_block();

        let mut result = vec![0u8; count];

        // Plan every device read before issuing any of them, so the runs can be
        // queued together rather than costing one round trip each. A range is
        // several runs whenever the file is fragmented, and whenever it is
        // longer than the 992 KiB one command can carry.
        let mut runs: Vec<ExtentRead> = Vec::new();
        let mut result_pos = 0usize;
        let mut remaining = count;
        let mut cur_byte = byte_offset;

        while remaining > 0 {
            let logical_block = (cur_byte / block_size) as u32;
            let offset_in_block = cur_byte % block_size;
            let blocks_needed = (remaining + offset_in_block).div_ceil(block_size) as u32;

            let run_blocks = match extents.run_at(logical_block) {
                BlockRun::Mapped { phys, blocks } => {
                    let bulk_blocks = blocks_needed.min(blocks).min(MAX_RUN_BLOCKS as u32);
                    let bulk_bytes = bulk_blocks as usize * block_size;
                    runs.push(ExtentRead {
                        lba: self.block_to_lba(phys),
                        sectors: (bulk_blocks * spb as u32) as u16,
                        dest: result_pos,
                        skew: offset_in_block,
                        len: remaining.min(bulk_bytes - offset_in_block),
                    });
                    bulk_blocks
                }
                // A hole reads as zeros, which `result` already holds.
                BlockRun::Hole { blocks } => {
                    let seen = EFS_EXTENT_HOLES.fetch_add(1, Ordering::Relaxed);
                    if seen < EXTENT_HOLE_LOG_LIMIT {
                        log!(
                            "efs: read hole at logical block {} (byte {}) of a {}-byte file, {} extents mapped",
                            logical_block,
                            cur_byte,
                            inode.size,
                            extents.as_slice().len()
                        );
                    }
                    blocks.unwrap_or(blocks_needed).min(blocks_needed)
                }
            };

            let run_bytes = run_blocks as usize * block_size - offset_in_block;
            let advance = remaining.min(run_bytes);
            result_pos += advance;
            remaining -= advance;
            cur_byte += advance;
        }

        // INVARIANT: file-data reads bypass BlockDevice to avoid shredding bulk
        // AHCI commands into per-page cache ops. The per-inode page cache owns
        // file data — do not route through BlockPageCache.
        let dev = block_io::lookup(self.device.device_id).ok_or(BlockError::DeviceGone)?;
        if !runs.is_empty() {
            EFS_EXTENT_READS.fetch_add(1, Ordering::Relaxed);
            EFS_EXTENT_RUNS.fetch_add(runs.len() as u64, Ordering::Relaxed);
        }
        let mut idx = 0usize;
        while idx < runs.len() {
            let mut end = idx;
            let mut batch_bytes = 0usize;
            while end < runs.len() && end - idx < MAX_INFLIGHT_READS {
                let bytes = runs[end].sectors as usize * 512;
                if end > idx && batch_bytes + bytes > MAX_INFLIGHT_BYTES {
                    break;
                }
                batch_bytes += bytes;
                end += 1;
            }
            let batch = &runs[idx..end];

            let staging: Vec<Arc<Vec<u8>>> = batch
                .iter()
                .map(|r| Arc::new(vec![0u8; r.sectors as usize * 512]))
                .collect();
            let reqs: Vec<_> = batch
                .iter()
                .zip(staging.iter())
                .map(|(r, buf)| (r.lba, r.sectors as u32, buf.clone()))
                .collect();

            EFS_EXTENT_BATCHES.fetch_add(1, Ordering::Relaxed);
            block_io::read_batch_blocking(&dev, &reqs)?;

            for (r, buf) in batch.iter().zip(staging.iter()) {
                result[r.dest..r.dest + r.len].copy_from_slice(&buf[r.skew..r.skew + r.len]);
            }
            idx = end;
        }

        Ok(result)
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
        let extents = self.load_extent_map(inode)?;
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
    /// Symbolic links are followed, including one named by the last component.
    fn resolve_path_inode(&self, path: &Path) -> Result<(u64, EfsInode), Error> {
        self.resolve_path_mode(path, LinkMode::Follow)
    }

    /// Resolve a path whose last component is left unfollowed, so a symbolic
    /// link resolves to the link itself. Links in the leading components are
    /// still followed.
    fn resolve_path_inode_nofollow(&self, path: &Path) -> Result<(u64, EfsInode), Error> {
        self.resolve_path_mode(path, LinkMode::NoFollow)
    }

    fn resolve_path_mode(&self, path: &Path, mode: LinkMode) -> Result<(u64, EfsInode), Error> {
        match self.resolve_path_walk(path, mode)? {
            Walk::Node(found) => Ok(found),
            Walk::Escaped(_) => Err(Error::LinkEscape),
        }
    }

    fn resolve_path_walk(&self, path: &Path, mode: LinkMode) -> Result<Walk, Error> {
        let components = path.components();
        let mut current_ino = EFS_ROOT_INO;
        let mut resolved: Vec<String> = Vec::new();

        for (index, name) in components.iter().enumerate() {
            let ino = match self.lookup_in_dir(current_ino, name.as_str())? {
                Some((ino, _)) => ino,
                None => return Err(Error::FileNotFound),
            };
            let inode = self.read_inode(ino)?;
            let is_last = index + 1 == components.len();

            if inode.mode & S_IFMT == S_IFLNK && (!is_last || mode.follows_final()) {
                let target = symlink_target(&inode)?;
                return Ok(Walk::Escaped(splice_symlink(
                    &resolved,
                    &target,
                    &components[index + 1..],
                )));
            }

            current_ino = ino;
            resolved.push(name.clone());
        }

        let inode = self.read_inode(current_ino)?;
        Ok(Walk::Node((current_ino, inode)))
    }
}

/// Read a symbolic link's target out of its inode. Targets are always stored
/// inline, since `symlink` refuses one longer than the inode data area.
fn symlink_target(inode: &EfsInode) -> Result<String, Error> {
    let len = inode.size as usize;
    if inode.flags & INODE_FLAG_INLINE_DATA == 0 || len > INODE_DATA_AREA_SIZE {
        return Err(Error::Corrupted);
    }
    core::str::from_utf8(&inode.data_area[..len])
        .map(ToString::to_string)
        .map_err(|_| Error::Corrupted)
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

        let end_offset = byte_offset + data.len();

        // `write_inline` and `convert_inline_to_extents` both put back a whole
        // inode built from this read, so it happens under the guard. The guard
        // is dropped before `write_via_extents`, which takes it per block.
        {
            let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);
            let inode = self.read_inode(ino)?;
            if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
                // Can we still fit inline?
                if end_offset <= INODE_DATA_AREA_SIZE {
                    let new_size = (end_offset as u64).max(inode.size);
                    return self.write_inline(ino, &inode, byte_offset, data, new_size, tx);
                }
                // Must convert to extent mode before writing.
                self.convert_inline_to_extents(ino, &inode, tx)?;
            }
        }

        self.write_via_extents(ino, byte_offset, data, tx)?;

        let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);
        let mut updated = self.read_inode(ino)?;
        if end_offset as u64 > updated.size {
            updated.size = end_offset as u64;
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
        // `inode.size` can legally exceed `INODE_DATA_AREA_SIZE` here: the
        // write-back `page_cache_write` updates `size` synchronously but
        // defers `flush_page` (which is the caller of this function), so
        // the on-disk inode can briefly say "inline + size 4096" even
        // though only the first 176 bytes of data_area are meaningful.
        // Preserve only the bytes that fit inline; `flush_page` will
        // overwrite the new block with the full 4 KiB cache page right
        // after we return.
        let preserved = (inode.size as usize).min(INODE_DATA_AREA_SIZE);
        let inline_data = inode.data_area[..preserved].to_vec();
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
        let mut written = 0usize;

        while written < data.len() {
            let cur_byte = byte_offset + written;
            let logical_block = (cur_byte / block_size) as u32;
            let offset_in_block = cur_byte % block_size;
            let copy_len = (data.len() - written).min(block_size - offset_in_block);

            let new = if copy_len == block_size {
                NewBlock::Overwritten
            } else {
                NewBlock::Zeroed
            };
            let phys_block = self.ensure_block_for_logical(ino, logical_block, new, tx)?;

            // INVARIANT: file-data writes bypass BlockPageCache to stay consistent
            // with the read path (read_via_extents) which also bypasses it. Only
            // metadata (inode, bitmap, BGD) goes through the journaled block cache.
            //
            // A block the write covers completely is not read first: there is
            // nothing of it left to preserve.
            let lba = self.block_to_lba(phys_block);
            let mut block_data = vec![0u8; block_size];
            if copy_len != block_size {
                block_read(self.device.device_id, lba, &mut block_data)?;
            }
            block_data[offset_in_block..offset_in_block + copy_len]
                .copy_from_slice(&data[written..written + copy_len]);
            block_write(self.device.device_id, lba, &block_data)?;

            written += copy_len;
        }
        Ok(())
    }

    /// Return the physical block for the given logical block, allocating if needed.
    /// Map `logical_block`, allocating a block if it is not mapped yet.
    ///
    /// Takes the inode read-modify-write guard; callers that already hold it
    /// must use [`Self::ensure_block_for_logical_locked`].
    fn ensure_block_for_logical(
        &self,
        ino: u64,
        logical_block: u32,
        new: NewBlock,
        tx: &mut TxHandle<'_>,
    ) -> Result<u64, Error> {
        let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);
        self.ensure_block_for_logical_locked(ino, logical_block, new, tx)
    }

    fn ensure_block_for_logical_locked(
        &self,
        ino: u64,
        logical_block: u32,
        new: NewBlock,
        tx: &mut TxHandle<'_>,
    ) -> Result<u64, Error> {
        let inode = self.read_inode(ino)?;
        let mut extents = self.load_extent_map(&inode)?;

        if let Some(phys) = extents.lookup(logical_block) {
            return Ok(phys);
        }

        // Allocate a new block, next to the file's own last extent when it can.
        let phys_block = self
            .alloc_blocks(1, extents.goal_for(logical_block), tx)?
            .first()
            .copied()
            .ok_or(Error::IoError)?;
        if new == NewBlock::Zeroed {
            let block_size = self.block_size() as usize;
            self.write_block(phys_block, &vec![0u8; block_size], tx)?;
        }

        extents.insert(logical_block, phys_block);

        let mut updated = inode;
        self.store_extent_map(&mut updated, &extents, tx)?;
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated, tx)?;

        Ok(phys_block)
    }

    /// Allocate / resolve physical blocks for a batch of logical block numbers,
    /// writing the updated inode ONCE at the end rather than once per block.
    ///
    /// Steps:
    ///   (a) Read the inode once.
    ///   (b) If the inline-data flag is set, convert to extent mode BEFORE the
    ///       loop — `ensure_block_for_logical` errors on inline inodes.
    ///   (c) Allocate every block the batch does not already map, in one request
    ///       so the allocator can answer it with one contiguous run.
    ///   (d) For each logical block, find an existing extent mapping or take the
    ///       next allocated block.  When a new block is contiguous with the last
    ///       extent (both logically and physically), extend that extent rather than
    ///       creating a new one (same coalescing logic as `ensure_block_for_logical`).
    ///   (e) Write the updated inode ONCE with the final extent list, updated
    ///       block count, optional new size, and refreshed checksum + mtime.
    ///
    /// Returns a Vec of physical block numbers in the same order as `logical_blocks`.
    ///
    /// Crash-safety note: `alloc_block` decrements the in-memory free-block
    /// counters and writes through `BlockPageCache` before the tx commits.  On tx
    /// abort the journal enrollment is discarded, but the `BlockPageCache` page
    /// may still flush, leaving the bitmap bit set on disk with no extent
    /// referencing it (leaked block).  This is the same failure mode as the
    /// per-page `flush_page` path — not new behaviour, and acceptable for v1.
    fn ensure_blocks_for_logical_batch(
        &self,
        ino: u64,
        logical_blocks: &[u32],
        new_size: Option<u64>,
        tx: &mut TxHandle<'_>,
    ) -> Result<Vec<u64>, Error> {
        if logical_blocks.is_empty() {
            return Ok(Vec::new());
        }
        let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);

        // (a) Read inode once.
        let mut inode = self.read_inode(ino)?;

        // (b) Convert inline data to extents before the loop if needed.
        //     `ensure_block_for_logical` (and our own extent walk below) requires
        //     extent mode.
        if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            self.convert_inline_to_extents(ino, &inode, tx)?;
            // Re-read so we have the updated data_area with the extent header.
            inode = self.read_inode(ino)?;
        }

        let mut extents = self.load_extent_map(&inode)?;
        let mut phys_blocks = Vec::with_capacity(logical_blocks.len());

        // (c) Allocate every unmapped block of the batch up front, so the run
        //     search sees the whole request and an appending file lands in one
        //     contiguous extent instead of the first free bit per block. The
        //     blocks are not zeroed: the caller writes every byte of each one
        //     straight to the device — see [`NewBlock`] for what staging those
        //     zeros costs. One bitmap write per run also collapses what used to
        //     be one `BlockPageCache::write_page` on the same page per block.
        let need = logical_blocks
            .iter()
            .filter(|&&lb| extents.lookup(lb).is_none())
            .count();
        // The goal is where the file's last extent ends, taken from the first
        // block the batch has to allocate.
        let goal = logical_blocks
            .iter()
            .find(|&&lb| extents.lookup(lb).is_none())
            .and_then(|&lb| extents.goal_for(lb));
        let mut pool: Vec<u64> = Vec::with_capacity(need);
        while pool.len() < need {
            // Each further round continues where the last run ended, so a
            // request the allocator could only answer in pieces still comes
            // back as few extents as the free space allows.
            let goal = pool.last().map(|b| b + 1).or(goal);
            let run = self.alloc_blocks(need - pool.len(), goal, tx)?;
            debug_assert!(!run.is_empty());
            pool.extend(run);
        }

        // (d) Walk logical blocks, reusing existing mappings or taking from the
        //     pool. Extent coalescing happens inside `insert`: a block contiguous
        //     with the last extent, logically and physically, extends it.
        let mut taken = 0;
        for &lb in logical_blocks {
            if let Some(phys) = extents.lookup(lb) {
                phys_blocks.push(phys);
                continue;
            }
            let phys_block = pool[taken];
            taken += 1;
            extents.insert(lb, phys_block);
            phys_blocks.push(phys_block);
        }
        // A repeated logical block in the batch leaves its share of the pool
        // unused; return it rather than leaking it.
        for &unused in &pool[taken..] {
            self.free_block(unused, tx)?;
        }

        // (e) Write updated inode ONCE with final extent list.
        self.store_extent_map(&mut inode, &extents, tx)?;
        if let Some(sz) = new_size
            && sz > inode.size
        {
            inode.size = sz;
        }
        inode.mtime_sec = current_unix_time();
        inode.checksum = checksum_inode(&inode);
        self.write_inode(ino, &inode, tx)?;

        Ok(phys_blocks)
    }
}

// ---- Bitmap operations --------------------------------------------------------

/// Blocks handed out by `alloc_block`, and blocks returned by `free_block`.
///
/// The gap between them, minus the blocks live files actually reference, is
/// how much space an allocation path lost track of. `efs-fsck` can only report
/// the total after the fact; these say which side it came from while the
/// system is running.
pub static EFS_BLOCKS_ALLOCATED: AtomicU64 = AtomicU64::new(0);
pub static EFS_BLOCKS_FREED: AtomicU64 = AtomicU64::new(0);
/// Allocation attempts that found no free block in any group.
pub static EFS_ALLOC_FAILED: AtomicU64 = AtomicU64::new(0);

/// Inodes put on and taken off the on-disk orphan chain.
///
/// The difference is the chain's current length, which is the number of inodes
/// a power cut right now would leave for the next mount to free. It tracks
/// `orphans_marked - orphans_dropped` in the same `/proc/efs_stats`, which is that
/// window seen from the VFS side; a lasting disagreement between the two pairs
/// means an unlink took a path that did not reach the chain.
pub static ORPHANS_LINKED: AtomicU64 = AtomicU64::new(0);
pub static ORPHANS_UNLINKED: AtomicU64 = AtomicU64::new(0);
/// Inodes freed by `process_orphan_list` at mount, i.e. deletions an unclean
/// shutdown interrupted. Non-zero after a power cut and zero after a clean one.
pub static ORPHANS_RECOVERED: AtomicU64 = AtomicU64::new(0);

/// What `read_via_extents` planned and what it cost the device.
///
/// `reads` counts the calls that reached the device at all, `runs` the
/// physically contiguous stretches they were split into, and `batches` the
/// submit-then-reap rounds those runs were queued in. The two ratios are what
/// the numbers are for: `runs / reads` above 1 means the file is fragmented, or
/// longer than the 992 KiB one command carries, and `runs / batches` above 1 is
/// the device round trips the queueing saved.
pub static EFS_EXTENT_READS: AtomicU64 = AtomicU64::new(0);
pub static EFS_EXTENT_RUNS: AtomicU64 = AtomicU64::new(0);
pub static EFS_EXTENT_BATCHES: AtomicU64 = AtomicU64::new(0);

/// Runs a file read planned as a hole below EOF: an unmapped logical block
/// inside the file's own size, which reads back as zeros. A sparse file reads
/// this way legitimately, so the counter is a signal rather than an error, but
/// nothing in this system writes sparse files today: a non-zero count on a file
/// written front to back means the extent map lost a block the data went to.
/// The first few are logged with the block that was missing.
pub static EFS_EXTENT_HOLES: AtomicU64 = AtomicU64::new(0);
/// Holes reported to the log, so a fragmented read does not flood it.
const EXTENT_HOLE_LOG_LIMIT: u64 = 8;

impl EfsDriver {
    /// Allocate a free block and return its absolute block number.
    fn alloc_block(&self, tx: &mut TxHandle<'_>) -> Result<u64, Error> {
        self.alloc_blocks(1, None, tx)?
            .first()
            .copied()
            .ok_or(Error::IoError)
    }

    /// Allocate up to `want` blocks, preferring a single physically contiguous
    /// run, and return them in ascending order.
    ///
    /// `goal` is the physical block the caller would like the run to start at —
    /// for file data, where the file's last extent ends. A run starting exactly
    /// there extends that extent instead of opening a new one, which is what
    /// keeps a file appended in several batches down to one extent; a file
    /// written in 8-block batches otherwise collects one extent per batch no
    /// matter how much contiguous space follows it. When the goal block is
    /// taken, the search falls back to first fit, starting at the goal's own
    /// group so the file at least stays near itself.
    ///
    /// Never returns more than `want` blocks and never returns none: a caller
    /// that needs more calls again, and a request that finds nothing free is an
    /// error. Asking for the whole run at once is what keeps an appending file
    /// in one extent once free space is fragmented, since taking the first free
    /// bit per block fills every small hole before reaching a run that could
    /// have held the file.
    fn alloc_blocks(
        &self,
        want: usize,
        goal: Option<u64>,
        tx: &mut TxHandle<'_>,
    ) -> Result<Vec<u64>, Error> {
        debug_assert!(want > 0);
        let block_size = self.block_size() as usize;

        let _bitmap = ranked_lock!(RANK_EFS_BITMAP, "EfsDriver.bitmap", self.bitmap_mutex);

        let (blocks_per_group, group_count) = {
            let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            (m.superblock.blocks_per_group as usize, m.bgd_table.len())
        };
        let bits_to_check = blocks_per_group.min(block_size * 8);

        // Snapshot per-group state without holding `mutable` across BPC I/O.
        let group_state = |g: usize| {
            let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            (
                m.bgd_table[g].free_blocks_count,
                m.bgd_table[g].block_bitmap_block,
            )
        };

        let goal_group = goal
            .map(|b| (b / blocks_per_group as u64) as usize)
            .filter(|&g| g < group_count);

        // The goal itself, when it is still free: take the run that starts
        // exactly there, however short, since even one block continues the
        // file's last extent.
        if let (Some(g), Some(goal_block)) = (goal_group, goal) {
            let (free_count, bitmap_block) = group_state(g);
            let bit = (goal_block % blocks_per_group as u64) as usize;
            if free_count > 0 && bit < bits_to_check {
                let bitmap = self.read_block(bitmap_block)?;
                let want_here = want.min(free_count as usize);
                let len = free_run_at(&bitmap, bits_to_check, want_here, bit);
                if len > 0 {
                    return self.claim_run(g, bitmap_block, bitmap, bit, len, tx);
                }
            }
        }

        // First fit, starting at the goal's group so a file that could not
        // extend its last extent still lands beside it rather than in the
        // first group with a free bit anywhere on the device.
        let first = goal_group.unwrap_or(0);
        for i in 0..group_count {
            let g = (first + i) % group_count;
            let (free_count, bitmap_block) = group_state(g);
            if free_count == 0 {
                continue;
            }
            let bitmap = self.read_block(bitmap_block)?;

            let want_here = want.min(free_count as usize);
            if let Some((bit, len)) = find_free_run(&bitmap, bits_to_check, want_here) {
                return self.claim_run(g, bitmap_block, bitmap, bit, len, tx);
            }
        }
        EFS_ALLOC_FAILED.fetch_add(1, Ordering::Relaxed);
        Err(Error::IoError)
    }

    /// Mark `len` blocks from `bit` of group `g` used, and enroll every metadata
    /// page the claim dirtied in `tx`. `bitmap` is the group's bitmap block as
    /// read; it is written back with the run set.
    fn claim_run(
        &self,
        g: usize,
        bitmap_block: u64,
        mut bitmap: Vec<u8>,
        bit: usize,
        len: usize,
        tx: &mut TxHandle<'_>,
    ) -> Result<Vec<u64>, Error> {
        let block_size = self.block_size() as usize;
        for b in bit..bit + len {
            set_bit(&mut bitmap, b);
        }

        let abs_block = {
            let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            let blocks_per_group = m.superblock.blocks_per_group as u64;
            m.bgd_table[g].free_blocks_count -= len as u64;
            m.superblock.free_blocks -= len as u64;
            g as u64 * blocks_per_group + bit as u64
        };

        // Write bitmap (enrolled by write_block).
        self.write_block(bitmap_block, &bitmap, tx)?;

        // Enroll BGD page (block 2 contains the BGD table; compute the
        // page holding group g's descriptor).
        let bgd_page_idx = {
            let bgds_per_block = block_size / BGD_SIZE;
            let bgd_block = 2u64 + (g / bgds_per_block) as u64;
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

        EFS_BLOCKS_ALLOCATED.fetch_add(len as u64, Ordering::Relaxed);
        Ok((0..len as u64).map(|i| abs_block + i).collect())
    }

    /// Free a block (by absolute block number).
    fn free_block(&self, block: u64, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let _bitmap = ranked_lock!(RANK_EFS_BITMAP, "EfsDriver.bitmap", self.bitmap_mutex);
        let block_size = self.block_size() as usize;
        let (group, bit, bitmap_block) = {
            let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
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
            let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
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

        EFS_BLOCKS_FREED.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Allocate a free inode and return its inode number (1-based).
    fn alloc_inode(&self, tx: &mut TxHandle<'_>) -> Result<u64, Error> {
        let block_size = self.block_size() as usize;

        let _bitmap = ranked_lock!(RANK_EFS_BITMAP, "EfsDriver.bitmap", self.bitmap_mutex);

        let (inodes_per_group, group_count) = {
            let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            (m.superblock.inodes_per_group as usize, m.bgd_table.len())
        };

        for g in 0..group_count {
            // Snapshot per-group state without holding `mutable` across BPC I/O.
            let (free_count, bitmap_block) = {
                let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
                (
                    m.bgd_table[g].free_inodes_count,
                    m.bgd_table[g].inode_bitmap_block,
                )
            };
            if free_count == 0 {
                continue;
            }
            let mut bitmap = self.read_block(bitmap_block)?;

            let bits_to_check = inodes_per_group.min(block_size * 8);
            if let Some(bit) = find_free_bit(&bitmap, bits_to_check) {
                set_bit(&mut bitmap, bit);

                let ino = {
                    let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
                    m.bgd_table[g].free_inodes_count -= 1;
                    m.superblock.free_inodes -= 1;
                    g as u64 * inodes_per_group as u64 + bit as u64 + 1
                };

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
        let _bitmap = ranked_lock!(RANK_EFS_BITMAP, "EfsDriver.bitmap", self.bitmap_mutex);
        let block_size = self.block_size() as usize;
        let inodes_per_group = self.inodes_per_group as usize;
        let ino0 = (ino - 1) as usize;
        let group = ino0 / inodes_per_group;
        let bit = ino0 % inodes_per_group;

        let bitmap_block = {
            let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            if group >= m.bgd_table.len() {
                return Err(Error::Corrupted);
            }
            m.bgd_table[group].inode_bitmap_block
        };

        let mut bitmap = self.read_block(bitmap_block)?;
        clear_bit(&mut bitmap, bit);
        self.write_block(bitmap_block, &bitmap, tx)?;

        {
            let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
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
            let phys_block =
                self.ensure_block_for_logical(dir_ino, logical_block, NewBlock::Overwritten, tx)?;
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
                if name_end <= dir_data.len() && &dir_data[name_start..name_end] == name.as_bytes()
                {
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
                            - find_prev_entry_len(&dir_data, prev_end, self.block_size() as usize);
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
            let phys_block =
                self.ensure_block_for_logical(dir_ino, logical_block, NewBlock::Overwritten, tx)?;
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
    let (chunks, remainder) = bitmap.as_chunks::<8>();
    for (chunk_idx, chunk) in chunks.iter().enumerate() {
        let val = u64::from_le_bytes(*chunk);
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

/// Find a free run of at most `want` bits within the first `max_bits`.
///
/// Returns the first run long enough to satisfy the request, and otherwise the
/// longest run in the bitmap, so a request lands in a small hole only when no
/// hole is big enough for it. With `want == 1` this is the first free bit.
fn find_free_run(bitmap: &[u8], max_bits: usize, want: usize) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut bit = 0;
    while bit < max_bits {
        if bit % 8 == 0 && bitmap[bit / 8] == 0xFF {
            bit += 8;
            continue;
        }
        if bitmap[bit / 8] & (1 << (bit % 8)) != 0 {
            bit += 1;
            continue;
        }
        let start = bit;
        while bit < max_bits && bit - start < want && bitmap[bit / 8] & (1 << (bit % 8)) == 0 {
            bit += 1;
        }
        let len = bit - start;
        if len >= want {
            return Some((start, len));
        }
        if best.is_none_or(|(_, best_len)| len > best_len) {
            best = Some((start, len));
        }
        bit += 1;
    }
    best
}

/// Length of the free run starting exactly at `start`, capped at `want`.
/// Zero when `start` itself is taken.
fn free_run_at(bitmap: &[u8], max_bits: usize, want: usize, start: usize) -> usize {
    let mut bit = start;
    while bit < max_bits && bit - start < want && bitmap[bit / 8] & (1 << (bit % 8)) == 0 {
        bit += 1;
    }
    bit - start
}

fn set_bit(bitmap: &mut [u8], bit: usize) {
    bitmap[bit / 8] |= 1 << (bit % 8);
}

fn clear_bit(bitmap: &mut [u8], bit: usize) {
    bitmap[bit / 8] &= !(1 << (bit % 8));
}

fn current_unix_time() -> u64 {
    // Use the kernel RTC for a reasonable timestamp.
    DateTime::now().to_unix_secs()
}

fn inode_to_file(name: String, inode: &EfsInode) -> File {
    let kind = if inode.mode & S_IFMT == S_IFDIR {
        FileKind::Directory
    } else if inode.mode & S_IFMT == S_IFREG {
        FileKind::File
    } else if inode.mode & S_IFMT == S_IFLNK {
        FileKind::Symlink
    } else if inode.mode & S_IFMT == S_IFIFO {
        FileKind::Fifo
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
        created: Some(FileTime::from_unix_secs(inode.ctime_sec)),
        accessed: Some(FileTime::from_unix_secs(inode.atime_sec)),
        modified: Some(FileTime::from_unix_secs(inode.mtime_sec)),
    }
}

fn new_inode(mode: u16, flags: u32) -> EfsInode {
    let mut inode = EfsInode::new(mode, (current_unix_time(), 0));
    inode.flags = flags;
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
        let parent = path.parent_or_root();

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

    fn create_fifo(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let name = path
            .last_component()
            .ok_or(Error::InvalidArgument)?
            .to_string();
        let parent = path.parent_or_root();

        let parent_ino = self.resolve_path(&parent)?;
        if self.read_inode(parent_ino)?.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }
        if self.lookup_in_dir(parent_ino, &name)?.is_some() {
            return Err(Error::AlreadyExists);
        }

        let mut tx = self.journal.begin_tx();
        match self.create_fifo_inner(parent_ino, &name, &mut tx) {
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
        let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
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
        let t0 = crate::timer::Instant::now();
        let tx = self.journal.begin_tx();
        drop(tx); // merges empty set — no-op on active tx
        self.journal
            .force_commit_and_wait()
            .map_err(|_| Error::IoError)?;
        let t1 = crate::timer::Instant::now();
        self.device.flush()?;
        let t2 = crate::timer::Instant::now();
        if t2.duration_since(t0).as_millis() >= 1_000 {
            log!(
                "efs flush_inode: slow: {} ms journal commit, {} ms device flush",
                t1.duration_since(t0).as_millis(),
                t2.duration_since(t1).as_millis()
            );
        }
        Ok(())
    }

    fn as_page_cache_ops(&self) -> Option<&dyn crate::fs::page_cache::PageCacheOps> {
        Some(self)
    }

    fn evict_inode(&self, ino: u64) -> Result<(), Error> {
        // Called from VfsInode::drop when the last Arc ref is released and
        // the inode was previously orphaned via remove_file. Free data blocks
        // + inode inside a journal transaction. Non-regular-file inos (dirs
        // etc.) shouldn't hit this path; if they do, silently tolerate.
        let file_inode = self.read_inode(ino)?;
        if file_inode.mode & S_IFMT != S_IFREG {
            return Ok(());
        }
        let mut tx = self.journal.begin_tx();
        // Off the chain in the same transaction that frees the storage: the two
        // together are what "this deletion finished" means on disk.
        match self
            .orphan_del(ino, &mut tx)
            .and_then(|()| self.free_file_storage(ino, &file_inode, &mut tx))
        {
            Ok(()) => Ok(()),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn create_dir(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let name = path.last_component().ok_or(Error::IoError)?.to_string();
        let parent = path.parent_or_root();

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
        let parent = path.parent_or_root();

        let parent_ino = self.resolve_path(&parent)?;
        // Unlinking a symbolic link removes the link, never its target.
        let (file_ino, file_inode) = self.resolve_path_inode_nofollow(&path)?;

        if !matches!(file_inode.mode & S_IFMT, S_IFREG | S_IFLNK | S_IFIFO) {
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
        let parent = path.parent_or_root();

        let parent_ino = self.resolve_path(&parent)?;
        // Unfollowed, so a symbolic link naming a directory is rejected as not
        // a directory rather than removing the directory it names.
        let (dir_ino, dir_inode) = self.resolve_path_inode_nofollow(&path)?;

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

    fn symlink(&self, target: &str, path: &Path) -> Result<(), Error> {
        if target.is_empty() || target.len() > INODE_DATA_AREA_SIZE {
            return Err(Error::InvalidArgument);
        }
        let path = path.normalize();
        let name = path
            .last_component()
            .ok_or(Error::InvalidArgument)?
            .to_string();
        let parent = path.parent_or_root();

        let parent_ino = self.resolve_path(&parent)?;
        if self.read_inode(parent_ino)?.mode & S_IFMT != S_IFDIR {
            return Err(Error::NotADir);
        }
        if self.lookup_in_dir(parent_ino, &name)?.is_some() {
            return Err(Error::InvalidArgument);
        }

        let mut tx = self.journal.begin_tx();
        match self.symlink_inner(parent_ino, &name, target, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn read_link(&self, path: &Path) -> Result<String, Error> {
        let path = path.normalize();
        let (_ino, inode) = self.resolve_path_inode_nofollow(&path)?;
        if inode.mode & S_IFMT != S_IFLNK {
            return Err(Error::InvalidArgument);
        }
        symlink_target(&inode)
    }

    fn link_escape(&self, path: &Path, mode: LinkMode) -> Result<LinkEscape, Error> {
        let path = path.normalize();
        match self.resolve_path_walk(&path, mode)? {
            Walk::Escaped(escape) => Ok(escape),
            Walk::Node(_) => Err(Error::Unsupported),
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

    fn file_info_nofollow(&self, path: &Path) -> Result<File, Error> {
        let path = path.normalize();
        let name = if path.is_root() {
            String::from("/")
        } else {
            path.last_component().unwrap_or("/").to_string()
        };
        let (_ino, inode) = self.resolve_path_inode_nofollow(&path)?;
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
        match self.truncate_inner(ino, size, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    fn set_times(&self, path: &Path, atime: Option<u64>, mtime: Option<u64>) -> Result<(), Error> {
        let path = path.normalize();
        let ino = self.resolve_path_inode(&path)?.0;

        // Whole-inode read-modify-write: read under the guard so a concurrent
        // size or extent update is not put back stale.
        let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);
        let mut updated = self.read_inode(ino)?;
        if let Some(secs) = atime {
            updated.atime_sec = secs;
            updated.atime_nsec = 0;
        }
        if let Some(secs) = mtime {
            updated.mtime_sec = secs;
            updated.mtime_nsec = 0;
        }
        updated.ctime_sec = current_unix_time();
        updated.ctime_nsec = 0;
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

    fn rename(&self, old_path: &Path, new_path: &Path) -> Result<(), Error> {
        let old_path = old_path.normalize();
        let new_path = new_path.normalize();

        let old_name = old_path.last_component().ok_or(Error::IoError)?.to_string();
        let new_name = new_path.last_component().ok_or(Error::IoError)?.to_string();

        let old_parent_path = old_path.parent_or_root();
        let new_parent_path = new_path.parent_or_root();

        let old_parent_ino = self.resolve_path(&old_parent_path)?;
        let new_parent_ino = self.resolve_path(&new_parent_path)?;
        // The name is what moves, not what it points at: renaming a symbolic
        // link must move the link.
        let (target_ino, target_inode) = self.resolve_path_inode_nofollow(&old_path)?;

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
    fn symlink_inner(
        &self,
        parent_ino: u64,
        name: &str,
        target: &str,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let new_ino = self.alloc_inode(tx)?;
        let mut inode = new_inode(S_IFLNK | 0o777, INODE_FLAG_INLINE_DATA);
        inode.data_area[..target.len()].copy_from_slice(target.as_bytes());
        inode.size = target.len() as u64;
        inode.checksum = checksum_inode(&inode);
        self.write_inode(new_ino, &inode, tx)?;
        self.add_dir_entry(parent_ino, name, new_ino, FT_SYMLINK, tx)?;
        Ok(())
    }

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

    fn create_fifo_inner(
        &self,
        parent_ino: u64,
        name: &str,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        let new_ino = self.alloc_inode(tx)?;
        // Size 0 and no extents, ever: a FIFO's bytes live in the kernel for as
        // long as an end is open and are never written down.
        let inode = new_inode(S_IFIFO | 0o644, INODE_FLAG_INLINE_DATA);
        self.write_inode(new_ino, &inode, tx)?;
        self.add_dir_entry(parent_ino, name, new_ino, FT_FIFO, tx)?;
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
            orphan_next: 0,
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

        // Update BGD used_dirs count and enroll the BGD page.
        let ipg = self.inodes_per_group as usize;
        let group = ((new_ino - 1) as usize) / ipg;
        {
            let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            if group < m.bgd_table.len() {
                m.bgd_table[group].used_dirs_count += 1;
            }
        }
        {
            let block_size = self.block_size() as usize;
            let bgds_per_block = block_size / BGD_SIZE;
            let bgd_block = 2u64 + (group / bgds_per_block) as u64;
            let bgd_page_idx = self.block_to_lba(bgd_block) / 8;
            if let Ok(guard) =
                BlockPageCache::global().read_page(self.device.device_id, bgd_page_idx)
            {
                tx.enroll_block(self.device.device_id, bgd_page_idx, guard.page_arc());
            }
        }

        self.add_dir_entry(parent_ino, name, new_ino, FT_DIR, tx)
    }

    /// Detach the dentry. For a regular file, block and inode freeing is
    /// deferred to `evict_inode`, which the VFS calls on the final
    /// `Arc<VfsInode>` drop. This is the Linux model: unlink removes the name
    /// but leaves data reachable by already-open fds and live mmap mappings.
    ///
    /// A symbolic link has no such deferral: `open` follows links, so no
    /// `VfsInode` ever names the link itself and nothing would ever evict it.
    /// Its storage is freed here, in the same transaction as the detach.
    fn remove_file_inner(
        &self,
        parent_ino: u64,
        file_ino: u64,
        file_inode: &EfsInode,
        name: &str,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        self.remove_dir_entry(parent_ino, name, tx)?;
        if file_inode.mode & S_IFMT == S_IFLNK {
            // Nothing can hold a reference to a link, so its storage goes in the
            // same transaction and it is never pending deletion.
            self.free_file_storage(file_ino, file_inode, tx)?;
        } else {
            // The inode outlives its last name until the final reference drops
            // (`vfs::remove_file`). The chain is what records that on disk, in
            // this same transaction, so a crash before the eviction is a
            // deletion to finish rather than a leak.
            self.orphan_add(file_ino, tx)?;
        }
        Ok(())
    }

    /// Free on-disk blocks + inode. Called from `FileSystem::evict_inode`
    /// (VfsInode::drop when orphan) and from `remove_dir_inner` (empty-dir
    /// removal, where there cannot be live refs).
    fn free_file_storage(
        &self,
        file_ino: u64,
        file_inode: &EfsInode,
        tx: &mut TxHandle<'_>,
    ) -> Result<(), Error> {
        if file_inode.flags & INODE_FLAG_INLINE_DATA == 0 {
            self.free_extent_storage(file_inode, tx)?;
        }
        self.free_inode(file_ino, tx)
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
        if dir_inode.flags & INODE_FLAG_INLINE_DATA == 0 {
            self.free_extent_storage(dir_inode, tx)?;
        }

        self.free_inode(dir_ino, tx)?;

        // Decrement parent link_count.
        let mut parent_inode = self.read_inode(parent_ino)?;
        if parent_inode.link_count > 0 {
            parent_inode.link_count -= 1;
        }
        parent_inode.checksum = checksum_inode(&parent_inode);
        self.write_inode(parent_ino, &parent_inode, tx)?;

        // Update BGD used_dirs and enroll the BGD page.
        let ipg = self.inodes_per_group as usize;
        let group = ((dir_ino - 1) as usize) / ipg;
        {
            let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            if group < m.bgd_table.len() && m.bgd_table[group].used_dirs_count > 0 {
                m.bgd_table[group].used_dirs_count -= 1;
            }
        }
        {
            let block_size = self.block_size() as usize;
            let bgds_per_block = block_size / BGD_SIZE;
            let bgd_block = 2u64 + (group / bgds_per_block) as u64;
            let bgd_page_idx = self.block_to_lba(bgd_block) / 8;
            if let Ok(guard) =
                BlockPageCache::global().read_page(self.device.device_id, bgd_page_idx)
            {
                tx.enroll_block(self.device.device_id, bgd_page_idx, guard.page_arc());
            }
        }

        self.remove_dir_entry(parent_ino, name, tx)
    }

    // ---- Orphan chain ------------------------------------------------------
    //
    // An inode that has lost its last name but whose storage is not freed yet is
    // on this chain. Without it that state exists only in memory (`VfsInode`'s
    // orphan mark), so an unclean shutdown inside the window strands the inode
    // and its blocks with nothing on disk saying they were pending deletion.
    // See `doc/efs.md` §14.

    /// Put `ino` at the head of the orphan chain, in `tx`.
    ///
    /// Both writes are in the caller's transaction, which is also the one that
    /// removes the directory entry: linking the inode in and unnaming it have to
    /// be the same atom, or a crash between them recreates the gap this closes.
    ///
    /// Stamping `orphan_next` rewrites the whole 256-byte inode, so it takes
    /// `inode_rmw` and reads the inode under it like every other whole-inode
    /// writer. An unlinked file can still be written through an open descriptor,
    /// and a concurrent `update_size` that read the inode first would otherwise
    /// put its copy back and take the chain link with it — truncating the chain
    /// and stranding everything below this inode.
    fn orphan_add(&self, ino: u64, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);
        let mut prevs = ranked_lock!(RANK_EFS_ORPHAN, "EfsDriver.orphan_prev", self.orphan_prev);

        let old_head = {
            let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            let old_head = m.superblock.last_orphan;
            m.superblock.last_orphan = ino as u32;
            old_head
        };

        let mut updated = self.read_inode(ino)?;
        updated.orphan_next = old_head;
        // No directory entry names it any more, and the on-disk count should say
        // so: a chained inode with a non-zero count reads to a checker as a name
        // it cannot find.
        updated.link_count = 0;
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated, tx)?;
        self.write_superblock(tx)?;

        prevs.insert(ino, 0);
        if old_head != 0 {
            prevs.insert(old_head as u64, ino);
        }
        ORPHANS_LINKED.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Unlink `ino` from the orphan chain, in `tx`. A no-op for an inode that is
    /// not on it, which is every directory and symbolic link: their storage is
    /// freed in the same transaction that unnames them, so they are never
    /// pending.
    /// Takes `inode_rmw` for the same reason [`orphan_add`] does: the predecessor
    /// it rewrites can be an unlinked-but-open file that another thread is
    /// writing.
    ///
    /// [`orphan_add`]: Self::orphan_add
    fn orphan_del(&self, ino: u64, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);
        let mut prevs = ranked_lock!(RANK_EFS_ORPHAN, "EfsDriver.orphan_prev", self.orphan_prev);
        let Some(prev) = prevs.remove(&ino) else {
            return Ok(());
        };

        let next = self.read_inode(ino)?.orphan_next;

        if prev == 0 {
            let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            m.superblock.last_orphan = next;
            drop(m);
            self.write_superblock(tx)?;
        } else {
            let mut prev_inode = self.read_inode(prev)?;
            prev_inode.orphan_next = next;
            prev_inode.checksum = checksum_inode(&prev_inode);
            self.write_inode(prev, &prev_inode, tx)?;
        }

        if next != 0 {
            prevs.insert(next as u64, prev);
        }
        ORPHANS_UNLINKED.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Finish the deletions the previous mount did not, and empty the chain.
    ///
    /// Runs once at mount, after journal replay: replay restores the chain the
    /// committed transactions describe, and only then does walking it mean
    /// anything. Each inode is freed in its own transaction and the head moves
    /// with it, so a crash part-way through leaves the rest of the chain intact
    /// for the next mount.
    ///
    /// Anything that stops the walk early — a cycle, an unreadable inode, a failed
    /// free — leaves the remainder chained and says so. The filesystem is
    /// consistent either way; it just still holds inodes that `efs-fsck --repair`
    /// would have to reclaim.
    fn process_orphan_list(&self) -> Result<(), Error> {
        let mut ino = {
            let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            m.superblock.last_orphan as u64
        };
        if ino == 0 {
            return Ok(());
        }

        // Every step frees an inode, so a cycle would free one twice and give its
        // blocks away. The visited set stops at the first repeat, before the
        // damage, rather than at a length bound after it.
        let mut visited: BTreeSet<u64> = BTreeSet::new();
        let mut freed = 0u64;
        while ino != 0 {
            if !visited.insert(ino) {
                log!(
                    "efs: orphan chain loops back to ino {}, stopping; run efs-fsck",
                    ino
                );
                break;
            }
            let inode = match self.read_inode(ino) {
                Ok(inode) => inode,
                Err(e) => {
                    log!(
                        "efs: orphan chain broken at ino {} ({:?}); run efs-fsck",
                        ino,
                        e
                    );
                    break;
                }
            };
            let next = inode.orphan_next as u64;

            let mut tx = self.journal.begin_tx();
            let result = self.free_file_storage(ino, &inode, &mut tx).and_then(|()| {
                let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
                m.superblock.last_orphan = next as u32;
                drop(m);
                self.write_superblock(&mut tx)
            });
            if let Err(e) = result {
                tx.abort();
                log!(
                    "efs: could not free orphan ino {} ({:?}); run efs-fsck",
                    ino,
                    e
                );
                break;
            }
            drop(tx);

            freed += 1;
            ORPHANS_RECOVERED.fetch_add(1, Ordering::Relaxed);
            ino = next;
        }

        if freed > 0 {
            log!(
                "efs: freed {} orphaned inode(s) left by an unclean shutdown",
                freed
            );
            self.journal.force_commit_and_wait()?;
        }
        Ok(())
    }

    /// Write the in-memory superblock to block 1 inside `tx`.
    ///
    /// Enrolling the superblock page without writing it, which several callers do,
    /// records whatever bytes the page already held; the orphan chain needs its
    /// head to be *durable* in the transaction that changed it, so it writes.
    fn write_superblock(&self, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        let block_size = self.block_size() as usize;
        let mut sb_block = vec![0u8; block_size];
        {
            let mut m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
            // Stamped here rather than by every path that changes a counter, so
            // the bytes that reach the disk always check out. `free_blocks` and
            // `free_inodes` move on every allocation, and nothing recomputed the
            // checksum for them before, which is why a live image so often had
            // `efs-fsck` reporting a superblock CRC mismatch it could repair.
            m.superblock.checksum = checksum_superblock(&m.superblock);
            let sb_bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(
                    &m.superblock as *const EfsSuperblock as *const u8,
                    core::mem::size_of::<EfsSuperblock>(),
                )
            };
            sb_block[..sb_bytes.len()].copy_from_slice(sb_bytes);
        }
        self.write_block(1, &sb_block, tx)
    }

    fn flush_inner(&self, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        self.write_superblock(tx)?;
        let block_size = self.block_size() as usize;

        let m = ranked_lock!(RANK_EFS_MUTABLE, "EfsDriver.mutable", self.mutable);
        // Write BGD table starting at block 2.
        let bgd_count = m.bgd_table.len();
        let bgds_per_block = block_size / BGD_SIZE;
        let bgd_blocks = bgd_count.div_ceil(bgds_per_block);

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

    fn truncate_inner(&self, ino: u64, size: u64, tx: &mut TxHandle<'_>) -> Result<(), Error> {
        // Rewrites the whole inode (size and extent list), so it takes the
        // same guard as the other read-modify-write paths and reads the inode
        // under it: a copy taken before the guard can already be stale.
        let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);
        let inode = &self.read_inode(ino)?;
        let current_size = inode.size;
        if size >= current_size {
            // Growing: the file stays sparse, but an inline inode must leave
            // inline mode first. Invariant: inline-mode inodes have
            // `size <= INODE_DATA_AREA_SIZE`, and `read_file_data` slices
            // `data_area` on the strength of it.
            let base = if inode.flags & INODE_FLAG_INLINE_DATA != 0
                && size > INODE_DATA_AREA_SIZE as u64
            {
                self.convert_inline_to_extents(ino, inode, tx)?;
                self.read_inode(ino)?
            } else {
                *inode
            };
            let mut updated = base;
            updated.size = size;
            updated.mtime_sec = current_unix_time();
            updated.checksum = checksum_inode(&updated);
            return self.write_inode(ino, &updated, tx);
        }

        // Shrinking: free excess blocks.
        if inode.flags & INODE_FLAG_INLINE_DATA == 0 {
            let block_size = self.block_size();
            let new_blocks = size.div_ceil(block_size);
            // An inode with no readable extent node has no blocks to release;
            // fall through to the plain size update rather than failing the
            // truncate.
            let extents = self.load_extent_map(inode).unwrap_or_default();
            let mut kept: Vec<EfsExtent> = Vec::with_capacity(extents.len());

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
                    kept.push(trimmed);
                } else {
                    kept.push(*ext);
                }
            }

            // Rebuild the map, which also collapses the tree back inline and
            // frees its now-surplus nodes once the survivors fit in the inode.
            let mut updated = *inode;
            self.store_extent_map(&mut updated, &ExtentMap::from_sorted(kept), tx)?;
            updated.size = size;
            updated.mtime_sec = current_unix_time();
            updated.checksum = checksum_inode(&updated);
            return self.write_inode(ino, &updated, tx);
        }

        // Inline or empty.
        let mut updated = *inode;
        updated.size = size;
        updated.mtime_sec = current_unix_time();
        updated.checksum = checksum_inode(&updated);
        self.write_inode(ino, &updated, tx)
    }

    // Every argument is a distinct field of the operation; grouping them into a
    // struct would only move the same list one level out.
    #[allow(clippy::too_many_arguments)]
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

        // POSIX has rename replace the destination. Without this the directory
        // ends up with two entries of one name, and every later lookup finds
        // whichever was written first -- so the rename appears to do nothing at
        // all, for good.
        if let Some((victim_ino, _)) = self.lookup_in_dir(new_parent_ino, new_name)?
            && victim_ino != target_ino
        {
            let victim = self.read_inode(victim_ino)?;
            if victim.mode & S_IFMT == S_IFDIR {
                // Replacing a directory is a different operation with its own
                // emptiness rules; reported as EISDIR rather than done wrong.
                return Err(Error::NotAFile);
            }
            self.remove_file_inner(new_parent_ino, victim_ino, &victim, new_name, tx)?;
        }

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
        let extents = self.load_extent_map(&inode)?;
        // An unmapped block inside the file is a hole and reads as zeros.
        let Some(phys_block) = extents.lookup(page_index as u32) else {
            buf.fill(0);
            return Ok(valid_bytes);
        };
        let lba = self.block_to_lba(phys_block);
        let spb = self.sectors_per_block();

        // INVARIANT: file-data page cache does not route through BlockDevice to avoid double-caching. Do not change.
        block_read(self.device.device_id, lba, &mut buf[..spb as usize * 512])?;
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

    /// Async prefetch: queue one AHCI read per physically contiguous run of the
    /// range and return their handles + shared buffers. A fragmented file is
    /// several runs, which is the common case for anything written alongside
    /// other files; only inline data and a range that maps nothing at all fall
    /// back to the sync `fill_pages_bulk`.
    fn submit_prefetch_pages(
        &self,
        ino: u64,
        offset: usize,
        count: usize,
    ) -> Result<Option<PrefetchPlan>, Error> {
        let inode = self.read_inode(ino)?;
        if inode.mode & S_IFMT != S_IFREG {
            return Err(Error::NotAFile);
        }
        // Past EOF: nothing to do.
        if offset >= inode.size as usize {
            return Ok(None);
        }
        let to_read = count.min(inode.size as usize - offset);
        if to_read == 0 {
            return Ok(None);
        }

        // Inline-data inodes can't prefetch — the data lives inside the inode.
        if inode.flags & INODE_FLAG_INLINE_DATA != 0 {
            return Ok(None);
        }

        // Prefetch is an optimisation, so a map this path cannot use is a
        // fallback to the sync fill rather than an error.
        let Ok(extents) = self.load_extent_map(&inode) else {
            return Ok(None);
        };

        let block_size = self.block_size() as usize;
        let logical_start = (offset / block_size) as u32;
        let logical_end = ((offset + to_read).div_ceil(block_size)) as u32;
        let spb = self.sectors_per_block();

        // Plan one read per contiguous run, stopping at the command budget:
        // the whole window is queued at once and nothing waits on it here, so
        // an unbounded plan would hand the device a queue's worth of
        // speculative reads ahead of the reader's own.
        struct PlannedRun {
            lba: u64,
            sectors: u32,
            page_offset: u64,
            blocks: u32,
        }
        let mut planned: Vec<PlannedRun> = Vec::new();
        let mut logical = logical_start;
        while logical < logical_end {
            let want = logical_end - logical;
            match extents.run_at(logical) {
                BlockRun::Mapped { phys, blocks } => {
                    if planned.len() == MAX_PREFETCH_RUNS {
                        break;
                    }
                    let run_blocks = want.min(blocks).min(MAX_RUN_BLOCKS as u32);
                    planned.push(PlannedRun {
                        lba: self.block_to_lba(phys),
                        sectors: run_blocks * spb as u32,
                        page_offset: (logical - logical_start) as u64,
                        blocks: run_blocks,
                    });
                    logical += run_blocks;
                }
                // A hole prefetches as zeros: no command, and finalization
                // leaves the page zeroed because no run covers it.
                BlockRun::Hole { blocks } => {
                    logical += blocks.unwrap_or(want).min(want);
                }
            }
        }

        if planned.is_empty() {
            return Ok(None);
        }
        let pages = (logical - logical_start) as u64;

        // Shared BlockBuffers so the DMA target stays alive even if no caller
        // ever observes the prefetch.
        let buffers: Vec<alloc::sync::Arc<alloc::vec::Vec<u8>>> = planned
            .iter()
            .map(|r| alloc::sync::Arc::new(alloc::vec![0u8; r.blocks as usize * block_size]))
            .collect();
        let reqs = planned
            .iter()
            .zip(buffers.iter())
            .map(|(r, buf)| {
                (
                    r.lba,
                    r.sectors,
                    crate::drivers::block_io::BlockBuffer::owned_vec(buf.clone()),
                )
            })
            .collect();

        let dev = crate::drivers::block_io::lookup(self.device.device_id).ok_or(Error::IoError)?;
        let handles = dev.submit_read_batch(reqs).map_err(|_| Error::IoError)?;

        let runs = handles
            .into_iter()
            .zip(buffers)
            .zip(planned.iter())
            .map(|((block_handle, buffer), r)| PrefetchRun {
                block_handle,
                buffer,
                page_offset: r.page_offset,
            })
            .collect();
        Ok(Some(PrefetchPlan { pages, runs }))
    }

    fn flush_page(
        &self,
        ino: u64,
        page_index: u64,
        buf: &[u8],
        _valid_bytes: usize,
    ) -> Result<(), Error> {
        // One guard for the whole convert-then-map sequence: both halves
        // rewrite the inode, and a concurrent `update_size` between them would
        // put the pre-conversion copy back.
        let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);
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
        let phys_block = match self.ensure_block_for_logical_locked(
            ino,
            logical_block,
            NewBlock::Overwritten,
            &mut tx,
        ) {
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
        if let Err(e) = block_write(self.device.device_id, lba, &buf[..needed]) {
            tx.abort();
            return Err(e.into());
        }
        // Written behind the block cache's back; drop its now-stale copy.
        BlockPageCache::global().invalidate_pages(self.device.device_id, lba / 8, 1);

        Ok(())
    }

    fn update_size(&self, ino: u64, new_size: u64) -> Result<(), Error> {
        // Whole-inode read-modify-write: without this guard a concurrent
        // `ensure_block*` writes its new extent list between our read and our
        // write, and stamping the size puts the old list back. The blocks it
        // allocated stay set in the bitmap with nothing pointing at them.
        let _rmw = ranked_lock!(RANK_EFS_INODE_RMW, "EfsDriver.inode_rmw", self.inode_rmw);
        let inode = self.read_inode(ino)?;
        if new_size <= inode.size {
            return Ok(());
        }
        let mut tx = self.journal.begin_tx();

        // Invariant: inline-mode inodes have `size <= INODE_DATA_AREA_SIZE`.
        // If the new size would overflow the inline area, convert to extent
        // mode BEFORE stamping the size — otherwise the on-disk inode would
        // transiently say "inline + size > 176", which any caller reading
        // via the on-disk path (loader prefetch, direct read_bytes_ino) would
        // interpret as an out-of-range slice on `data_area`.
        let base = if inode.flags & INODE_FLAG_INLINE_DATA != 0
            && new_size > INODE_DATA_AREA_SIZE as u64
        {
            if let Err(e) = self.convert_inline_to_extents(ino, &inode, &mut tx) {
                tx.abort();
                return Err(e);
            }
            // Re-read so the updated inode carries the extent header and
            // cleared INLINE_DATA flag.
            match self.read_inode(ino) {
                Ok(v) => v,
                Err(e) => {
                    tx.abort();
                    return Err(e);
                }
            }
        } else {
            inode
        };

        let mut updated = base;
        updated.size = new_size;
        updated.mtime_sec = current_unix_time();
        updated.checksum = efs_common::checksum_inode(&updated);
        match self.write_inode(ino, &updated, &mut tx) {
            Ok(v) => Ok(v),
            Err(e) => {
                tx.abort();
                Err(e)
            }
        }
    }

    /// Flush a batch of dirty pages in a single journal transaction per chunk.
    ///
    /// Batch size is capped at `MAX_BULK_PAGES` (512) pages.  If `pages` is
    /// larger than that, we process 512-page chunks, each with its own tx.
    /// `new_size_hint` is forwarded only to the last chunk so the inode size
    /// is written exactly once.
    ///
    /// Within each chunk:
    ///   1. Open one `TxHandle`.
    ///   2. Resolve / allocate all physical blocks in one pass via
    ///      `ensure_blocks_for_logical_batch`, which reads and writes the inode
    ///      once regardless of how many pages are in the batch.
    ///   3. Write each page's data directly to AHCI (bypassing BlockPageCache,
    ///      consistent with the single-page `flush_page` path).
    ///   4. Drop the tx on success (merges into the active journal tx).
    ///      On any error, abort the tx and return immediately.
    ///
    /// Crash-safety: mid-batch AHCI failure → tx.abort() → bitmap bits may
    /// still flush via BlockPageCache (same block-leak failure mode as the
    /// per-page `flush_page` path — not new behaviour).  The inode write is
    /// the last step in `ensure_blocks_for_logical_batch`, so a failure before
    /// `write_inode` leaves no extent records on disk.
    fn flush_pages_bulk(
        &self,
        ino: u64,
        pages: &[(u64, Arc<CachedPage>)],
        new_size_hint: Option<u64>,
    ) -> Result<(), Error> {
        const MAX_BULK_PAGES: usize = 512;
        let spb = self.sectors_per_block();
        let needed_bytes = spb as usize * 512;

        let mut chunk_start = 0usize;
        while chunk_start < pages.len() {
            let chunk_end = (chunk_start + MAX_BULK_PAGES).min(pages.len());
            let chunk = &pages[chunk_start..chunk_end];
            let is_last_chunk = chunk_end == pages.len();

            let logical_blocks: Vec<u32> = chunk.iter().map(|(pi, _)| *pi as u32).collect();

            let size_for_chunk = if is_last_chunk { new_size_hint } else { None };

            let mut tx = self.journal.begin_tx();

            let phys_blocks = match self.ensure_blocks_for_logical_batch(
                ino,
                &logical_blocks,
                size_for_chunk,
                &mut tx,
            ) {
                Ok(v) => v,
                Err(e) => {
                    tx.abort();
                    return Err(e);
                }
            };

            // Write file data directly to AHCI — do not route through
            // BlockPageCache (same invariant as single-page flush_page).
            //
            // One command per run of physically contiguous blocks, not one per
            // page. Issuing a command costs far more than the sectors it
            // carries, and a sequential file's blocks are contiguous, so this
            // is the difference between one command and several hundred.
            // Cache pages are separate frames, so each run is assembled into a
            // staging buffer first; the copy is a fraction of the commands it
            // replaces.
            //
            // Runs are issued back to back and waited on afterwards, so the
            // drive sees a queue rather than one command per round trip. Depth
            // is capped below `OWNED_OPS_CAP` so every outstanding command
            // still gets its cancellation hookup, and each staging buffer is
            // held until its own command completes because the DMA reads from
            // it.
            const MAX_INFLIGHT_WRITES: usize = 16;
            let mut inflight: VecDeque<InflightWrite> = VecDeque::new();
            let mut failure: Option<Error> = None;

            let mut run_start = 0usize;
            while run_start < chunk.len() {
                let mut run_len = 1usize;
                while run_start + run_len < chunk.len()
                    && run_len < MAX_RUN_BLOCKS
                    && phys_blocks[run_start + run_len] == phys_blocks[run_start + run_len - 1] + 1
                {
                    run_len += 1;
                }

                let mut staging = vec![0u8; run_len * needed_bytes];
                for i in 0..run_len {
                    let buf = unsafe { chunk[run_start + i].1.as_slice() };
                    staging[i * needed_bytes..(i + 1) * needed_bytes]
                        .copy_from_slice(&buf[..needed_bytes]);
                }
                let staging = Arc::new(staging);

                while inflight.len() >= MAX_INFLIGHT_WRITES {
                    let Some(done) = inflight.pop_front() else {
                        break;
                    };
                    if let Err(e) = reap_write(self.device.device_id, done) {
                        failure.get_or_insert(e.into());
                    }
                }

                let lba = self.block_to_lba(phys_blocks[run_start]);
                let sectors = spb as usize * run_len;
                match submit_block_write(
                    self.device.device_id,
                    lba,
                    sectors as u16,
                    staging.clone(),
                ) {
                    Ok(handle) => inflight.push_back(InflightWrite {
                        handle,
                        staging,
                        lba,
                        sectors: sectors as u16,
                        first_page: lba / 8,
                        pages: run_len as u64,
                    }),
                    Err(e) => {
                        failure.get_or_insert(e.into());
                        break;
                    }
                }

                run_start += run_len;
            }

            // Every command must be drained before its staging buffer is
            // dropped, so a failed run waits for the ones already issued
            // rather than returning out from under their DMA.
            while let Some(done) = inflight.pop_front() {
                if let Err(e) = reap_write(self.device.device_id, done) {
                    failure.get_or_insert(e.into());
                }
            }

            if let Some(e) = failure {
                tx.abort();
                return Err(e);
            }

            // tx drops here, merging enrolled metadata into the active journal tx.
            drop(tx);
            chunk_start = chunk_end;
        }

        Ok(())
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
