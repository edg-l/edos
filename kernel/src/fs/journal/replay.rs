// Journal replay: mount-time recovery.
//
// Two passes over the journal ring:
//   Pass 1: walk the ring from the tail, collecting committed transactions and
//           the revokes that veto individual blocks. This is
//           `efs_common::scan_committed`, shared with efs-fsck so the checker
//           and the kernel cannot disagree about where the live region ends.
//   Pass 2: write each committed transaction's data blocks to their home FS
//           locations, skipping revoked ones.
//
// After replay, flush all writes, then reset the JSB from the cursors the scan
// reported. Replay is idempotent: a crash during replay re-replays the same
// transactions on next mount, because the JSB tail is not advanced until the end.

use alloc::{vec, vec::Vec};

use efs_common::{ScanStop, replay_write, scan_committed};

use crate::{
    drivers::{
        ahci::AhciError,
        block_io::{self, BlockBuffer, WriteFlags},
    },
    log,
};

fn block_read(device_id: u64, lba: u64, sectors: u16, buf: &mut [u8]) -> Result<(), AhciError> {
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    // SAFETY: `h.wait()?` below reaps this op before returning.
    let h = dev.submit_read(lba, sectors as u32, unsafe {
        BlockBuffer::reaped_by_submitter(buf.as_mut_ptr(), buf.len())
    })?;
    h.wait()?;
    Ok(())
}

/// A replay write that has been submitted but not waited on.
type InflightReplay = alloc::sync::Arc<crate::drivers::block_io::BlockIoHandle>;

/// Issue a home-block write without waiting, so replay can keep several
/// outstanding instead of paying a round trip per block. The op co-owns
/// `data` via the `Arc`, so it stays valid until the device is done with it
/// whether or not replay itself is still around to wait on the handle.
fn submit_block_write(
    device_id: u64,
    lba: u64,
    sectors: u16,
    data: Vec<u8>,
) -> Result<InflightReplay, AhciError> {
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    dev.submit_write(
        lba,
        sectors as u32,
        BlockBuffer::owned_vec(alloc::sync::Arc::new(data)),
        WriteFlags::NONE,
    )
    .map_err(Into::into)
}

/// Wait for one outstanding replay write.
fn reap_replay(write: InflightReplay) -> Result<(), AhciError> {
    write.wait()?;
    Ok(())
}

fn block_flush(device_id: u64) -> Result<(), AhciError> {
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    let h = dev.submit_flush()?;
    h.wait()?;
    Ok(())
}

const BLOCK_SIZE: usize = 4096;
const SECTORS_PER_BLOCK: u16 = 8;

/// Result of replay: how many transactions were applied, and where the live
/// region ended.
///
/// The end cursor is reported because the superblock's head cannot supply it:
/// it is advisory, and after a crash it can name a position older than the
/// transactions replay just applied. Restarting the journal there would hand out
/// sequence numbers that are already on disk and write over ring blocks still
/// holding live data.
pub struct ReplayResult {
    pub txs_applied: u64,
    /// Sequence number the next transaction should take: one past the last
    /// transaction applied, or `tail_seq` when nothing was.
    pub next_seq: u64,
    /// Ring cursor just past the last applied transaction, or `tail_block`
    /// when nothing was applied.
    pub next_block: u64,
}

/// Replay committed journal transactions to their home FS locations.
///
/// `device_id`: AHCI device.
/// `first_block`: absolute EFS block number of the journal region start.
/// `block_count`: total journal blocks (including the JSB at block 0).
/// `partition_start_lba`: starting LBA of the EFS partition (for fs_block → LBA).
/// `tail_seq`, `tail_block`: from the validated JournalSuperblock.
///
/// The scan is bounded by sequence continuity, not by a persisted head; see
/// [`efs_common::journal_scan`] for why, and for the rule efs-fsck shares.
///
/// Returns the number of transactions replayed. Every mount scans; on a
/// genuinely clean journal the block at `tail_block` fails the continuity
/// check immediately and this costs one block read.
pub fn replay(
    device_id: u64,
    first_block: u64,
    block_count: u32,
    partition_start_lba: u64,
    tail_seq: u64,
    tail_block: u64,
) -> Result<ReplayResult, AhciError> {
    log!(
        "efs journal: replay scan start tail_seq={} tail_block={}",
        tail_seq,
        tail_block
    );

    // ---- Pass 1: collect committed transactions and their revokes -----------
    //
    // `first_block` is an EFS block number, which is partition-relative, so the
    // partition's starting LBA has to be added. Without it the ring is read
    // `partition_start_lba` sectors too low, off the front of the partition:
    // every header fails to parse, replay reports no committed transactions,
    // and an unclean mount silently discards the metadata the journal was
    // holding for it. The home-block write in pass 2 is the other addressing
    // domain and must not add it.
    let scan = scan_committed(
        tail_seq,
        tail_block,
        block_count as u64 - 1, // block 0 of the region is the JSB
        BLOCK_SIZE,
        |region_block: u64| -> Result<Vec<u8>, AhciError> {
            let lba = partition_start_lba + (first_block + region_block) * SECTORS_PER_BLOCK as u64;
            let mut buf = vec![0u8; BLOCK_SIZE];
            block_read(device_id, lba, SECTORS_PER_BLOCK, &mut buf)?;
            Ok(buf)
        },
    )?;

    match scan.stop {
        ScanStop::PartialTx { seq } => log!(
            "efs journal: partial tx seq={} (no commit block), stopping replay scan",
            seq
        ),
        ScanStop::CommitCrcMismatch {
            seq,
            found,
            expected,
        } => log!(
            "efs journal: tx seq={} commit CRC mismatch (got {:#x}, expected {:#x}), stopping",
            seq,
            found,
            expected
        ),
        ScanStop::RingBound { expected } => log!(
            "efs journal: replay scan hit the ring-size bound at seq={} without a continuity break, stopping",
            expected
        ),
        ScanStop::DegenerateRing => log!("efs journal: block_count is 0; the ring holds nothing"),
        // A sequence break, a non-journal block or a non-descriptor is the
        // ordinary end of the live region and says nothing worth logging.
        ScanStop::SequenceBreak { .. }
        | ScanStop::NotJournalBlock
        | ScanStop::NotDescriptor { .. } => {}
    }

    if scan.txs.is_empty() {
        log!("efs journal: scanned, nothing to replay");
        return Ok(ReplayResult {
            txs_applied: 0,
            next_seq: tail_seq,
            next_block: tail_block,
        });
    }

    // ---- Pass 2: apply committed txs ----------------------------------------
    //
    // Home-block writes are queued rather than waited on one at a time: replay
    // runs on the mount path, so its cost is boot latency after an unclean
    // shutdown, and a full ring is hundreds of blocks. Depth is bounded so the
    // port's queue is not oversubscribed and so the buffers held for DMA stay a
    // fixed cost rather than the whole ring.
    //
    // Ordering within the ring is preserved by the revoke check below and by
    // replay applying transactions in sequence order, not by the device: two
    // outstanding writes never target the same home block, because a later
    // transaction writing the same block revokes the earlier one.
    const MAX_INFLIGHT_REPLAY: usize = 16;
    let mut applied = 0u64;
    let mut inflight: alloc::collections::VecDeque<InflightReplay> =
        alloc::collections::VecDeque::new();
    let mut failure: Option<AhciError> = None;

    'outer: for tx in &scan.txs {
        for i in 0..tx.entries.len() {
            // Yields nothing for a block a later transaction revoked, and
            // un-escapes one whose descriptor entry is marked ESCAPED.
            let Some((fs_block, data)) = replay_write(tx, i, &scan.revokes) else {
                continue;
            };

            while inflight.len() >= MAX_INFLIGHT_REPLAY {
                let Some(done) = inflight.pop_front() else {
                    break;
                };
                if let Err(e) = reap_replay(done) {
                    failure.get_or_insert(e);
                    break 'outer;
                }
            }

            // Write to home location via direct AHCI (not through the block page
            // cache — the cache isn't populated yet during early mount).
            //
            // `fs_block` is device-absolute, not partition-relative, and
            // `DescriptorEntry` documents it as such: enrolment stamps the
            // block page cache's page index, which EFS derives as
            // `block_to_lba(block) / SECTORS_PER_BLOCK` and so already carries
            // the partition offset. Adding `partition_start_lba` here a second
            // time put every home write exactly `partition_start_lba /
            // SECTORS_PER_BLOCK` blocks too high, dropping inode-table content
            // onto the first data block. The ring read above is the other
            // domain and does need the offset, because `first_block` is
            // partition-relative.
            let lba = fs_block * SECTORS_PER_BLOCK as u64;
            match submit_block_write(device_id, lba, SECTORS_PER_BLOCK, data) {
                Ok(w) => inflight.push_back(w),
                Err(e) => {
                    failure.get_or_insert(e);
                    break 'outer;
                }
            }
        }
        applied += 1;
    }

    // Every outstanding command is drained before its buffer is dropped, on the
    // failure path too: returning early would free memory the device is still
    // reading from.
    while let Some(done) = inflight.pop_front() {
        if let Err(e) = reap_replay(done) {
            failure.get_or_insert(e);
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }

    // Flush all replayed writes to platter.
    block_flush(device_id)?;

    log!(
        "efs journal: replayed {} transactions ({} ring blocks)",
        applied,
        scan.ring_blocks
    );

    Ok(ReplayResult {
        txs_applied: applied,
        next_seq: scan.next_seq,
        next_block: scan.next_block,
    })
}
