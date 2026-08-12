//! Journal replay for efs-fsck.
//!
//! The ring walk itself is `efs_common::scan_committed`, the same code the
//! kernel runs at mount time; only reading and writing blocks differs. Sharing
//! it is what keeps the checker's verdict and the kernel's recovery from
//! drifting apart, which they did: the checker used to bound the walk by the
//! journal superblock's advisory head and to add the partition offset to home
//! blocks that already carried it.
//!
//! Pass 2 applies each committed transaction's data blocks to their home
//! locations, respecting revokes and un-escaping, then rewrites the JSB from
//! the cursors the scan reported so a second invocation is a provable no-op.

use std::io;

use efs_common::{
    EfsSuperblock, JournalScan, JournalSuperblock, journal_sb_checksum, replay_write,
    scan_committed,
};

use crate::disk::Disk;

/// Outcome of a replay run.
pub struct ReplayResult {
    pub tx_count: u32,
    pub blocks_written: u32,
}

/// Walk the ring from the recorded tail and collect every transaction that is
/// fully committed, together with the revoke set that applies to them.
///
/// This is what decides whether a journal has work outstanding;
/// `JournalScan::is_dirty` reads the answer off it. `tail_seq != head_seq` does
/// not decide it, in either direction: `head_seq` names the open transaction,
/// which is never committed, so a clean journal normally sits one apart, and a
/// crash between a commit and the superblock write leaves the two equal with a
/// committed transaction still in the ring.
pub fn scan(disk: &mut Disk, sb: &EfsSuperblock) -> io::Result<JournalScan> {
    let first_block = sb.journal_first_block;
    let block_size = sb.block_size() as usize;

    let jsb: JournalSuperblock = disk.read_struct_at(first_block, 0)?;
    let tail_seq = jsb.tail_seq;
    let tail_block = jsb.tail_block;
    let ring_size = (jsb.block_count as u64).saturating_sub(1);

    scan_committed(
        tail_seq,
        tail_block,
        ring_size,
        block_size,
        |region_block: u64| disk.read_block(first_block + region_block),
    )
}

/// Apply a scan's committed transactions to their home locations and retire
/// them from the ring.
pub fn replay(disk: &mut Disk, sb: &EfsSuperblock, scan: &JournalScan) -> io::Result<ReplayResult> {
    let first_block = sb.journal_first_block;
    let block_size = sb.block_size() as u64;

    // Home blocks are device-absolute, so a partition that does not start on a
    // block boundary cannot be addressed in that domain at all: the kernel's
    // own `block_to_lba(block) / sectors_per_block` truncates. Refusing is the
    // only honest answer, since replaying at a truncated offset would scatter
    // metadata over file data.
    if !disk.partition_offset.is_multiple_of(block_size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "partition offset {} is not a multiple of the block size {}; \
                 journal home blocks cannot be addressed",
                disk.partition_offset, block_size
            ),
        ));
    }

    let mut blocks_written: u32 = 0;

    for tx in &scan.txs {
        for i in 0..tx.entries.len() {
            let Some((fs_block, data)) = replay_write(tx, i, &scan.revokes) else {
                continue;
            };
            disk.write_device_block(fs_block, &data)?;
            blocks_written += 1;
        }
    }

    disk.fsync()?;

    // Retire the replayed region: both cursors move to where the scan found the
    // live region to end. Writing the superblock's own head back here would
    // reinstate a cursor that is stale by design, leaving the kernel to replay
    // the same transactions again on the next mount.
    let jsb: JournalSuperblock = disk.read_struct_at(first_block, 0)?;
    let mut new_jsb = jsb;
    new_jsb.tail_seq = scan.next_seq;
    new_jsb.head_seq = scan.next_seq;
    new_jsb.tail_block = scan.next_block;
    new_jsb.head_block = scan.next_block;
    new_jsb.crc32 = journal_sb_checksum(&new_jsb);

    disk.write_struct_at(first_block, 0, &new_jsb)?;
    disk.fsync()?;

    Ok(ReplayResult {
        tx_count: scan.txs.len() as u32,
        blocks_written,
    })
}
