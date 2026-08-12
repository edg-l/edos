// Journal replay: mount-time recovery.
//
// Two-pass scan of the journal ring:
//   Pass 1: build revoke set — for each REVOKE block, record (fs_block, max_revoke_seq).
//   Pass 2: apply committed transactions — for each DESCRIPTOR+COMMIT pair,
//           write data blocks to their home FS locations unless revoked.
//
// After replay, flush all writes, then reset the JSB (tail = head).
// Replay is idempotent: a crash during replay re-replays the same txs
// on next mount because the JSB tail is not advanced until the end.

use alloc::{collections::BTreeMap, vec, vec::Vec};

use efs_common::{
    DESC_FLAG_ESCAPED, JOURNAL_BLOCK_MAGIC, JournalBlockHeader, JournalBlockKind,
    commit_block_checksum, parse_descriptor_entries, parse_header, parse_revoke_entries,
};

use crate::{
    drivers::{
        ahci::AhciError,
        block_io::{self, BlockBuffer, WriteFlags},
    },
    log,
};

fn block_read(device_id: u64, lba: u64, sectors: u16, buf: &mut [u8]) -> Result<(), AhciError> {
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    let h = dev.submit_read(
        lba,
        sectors as u32,
        BlockBuffer::Slice {
            ptr: buf.as_mut_ptr(),
            len: buf.len(),
        },
    )?;
    h.wait()?;
    Ok(())
}

/// A replay write that has been submitted but not waited on. `data` is the DMA
/// source and must outlive the handle, so it is carried here rather than being
/// dropped at the end of the loop iteration that issued it.
struct InflightReplay {
    handle: alloc::sync::Arc<crate::drivers::block_io::BlockIoHandle>,
    data: Vec<u8>,
}

/// Issue a home-block write without waiting, so replay can keep several
/// outstanding instead of paying a round trip per block.
fn submit_block_write(
    device_id: u64,
    lba: u64,
    sectors: u16,
    data: Vec<u8>,
) -> Result<InflightReplay, AhciError> {
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    let handle = dev.submit_write(
        lba,
        sectors as u32,
        BlockBuffer::Slice {
            ptr: data.as_ptr() as *mut u8,
            len: data.len(),
        },
        WriteFlags::NONE,
    )?;
    Ok(InflightReplay { handle, data })
}

/// Wait for one outstanding replay write. The buffer is only free once the
/// device has finished reading it.
fn reap_replay(write: InflightReplay) -> Result<(), AhciError> {
    let result = write.handle.wait();
    drop(write.data);
    result?;
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
/// The end cursor is reported because the superblock's head cannot supply it.
/// That head is written only when the tail advances, so after a crash it names
/// a position older than the transactions replay just applied. Restarting the
/// journal there would hand out sequence numbers that are already on disk and
/// write over ring blocks still holding live data.
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
/// The scan is bounded by sequence continuity, not by a persisted head: the
/// journal superblock's `head_seq`/`head_block` are written only when the
/// tail advances (`Journal::advance_tail`), so they can lag arbitrarily far
/// behind transactions that committed since the last checkpoint. Trusting
/// them as a scan bound is what silently dropped committed-but-uncheckpointed
/// work on recovery. Sequence numbers are global and never reused, so walking
/// forward from the tail and stopping at the first break in the sequence
/// finds exactly the live region regardless of what the superblock recorded.
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

    let ring_size = block_count as u64 - 1; // block 0 = JSB

    // Read one journal ring block by its ring-internal index.
    //
    // `first_block` is an EFS block number, which is partition-relative, so the
    // partition's starting LBA has to be added exactly as the home-block write
    // below does. Without it the ring is read `partition_start_lba` sectors too
    // low, off the front of the partition: every header fails to parse, replay
    // reports no committed transactions, and an unclean mount silently discards
    // the metadata the journal was holding for it.
    let read_ring_block = |ring_idx: u64| -> Result<Vec<u8>, AhciError> {
        let wrapped = (ring_idx % ring_size) + 1;
        let lba = partition_start_lba + (first_block + wrapped) * SECTORS_PER_BLOCK as u64;
        let mut buf = vec![0u8; BLOCK_SIZE];
        block_read(device_id, lba, SECTORS_PER_BLOCK, &mut buf)?;
        Ok(buf)
    };

    // ---- Pass 1: scan for committed txs and build revoke set ----------------
    // We scan the ring sequentially, parsing descriptor→data→revoke→commit
    // groups. A tx is committed iff we find a COMMIT block with matching seq.
    // We also collect revoke entries.

    let mut revoke_set: BTreeMap<u64, u64> = BTreeMap::new(); // fs_block → max_revoke_seq
    let mut committed_txs: Vec<CommittedTx> = Vec::new();
    let mut ring_pos: u64 = tail_block;
    let mut total_ring_blocks: u64 = 0;
    let mut expected_seq: u64 = tail_seq;
    // Advanced only past a transaction that was fully accepted. `ring_pos`
    // cannot serve: a transaction rejected at its commit block has already
    // moved it past that transaction's descriptor and data.
    let mut live_end_pos: u64 = tail_block;

    // Sequence numbers are global and never reused, so a transaction whose
    // descriptor carries anything other than `expected_seq` is either the
    // stale far side of a wrapped ring (an older transaction that still
    // parses cleanly, with a lower seq — replaying it would roll metadata
    // backwards) or genuinely not there. Either way the live region ends
    // here. This replaces bounding the scan by the persisted head, which
    // lags behind whatever committed since the last checkpoint.
    //
    // The block count is capped at `ring_size` regardless: a ring cannot
    // hold more live transactions than it has blocks, so hitting this bound
    // without a continuity break means something is corrupt.
    loop {
        if ring_pos >= tail_block + ring_size {
            log!(
                "efs journal: replay scan hit the ring-size bound at seq={} without a continuity break, stopping",
                expected_seq
            );
            break;
        }

        let block = read_ring_block(ring_pos)?;
        let hdr = parse_header(&block);

        let Some(hdr) = hdr else {
            // Not a valid journal block — end of committed data.
            break;
        };

        if hdr.kind != JournalBlockKind::Descriptor as u8 {
            // Expected a descriptor at the start of a tx. Stop.
            break;
        }

        if hdr.seq != expected_seq {
            // Continuity broken: either the ring's stale far side (a lower
            // seq left over from before the last wrap) or the true end of
            // the live region.
            break;
        }

        let desc_seq = hdr.seq;
        let desc_tx_id = hdr.tx_id;

        // Parse descriptor entries to know how many data blocks follow.
        let entries = parse_descriptor_entries(&block, BLOCK_SIZE);
        let n_data = entries.len() as u64;
        ring_pos += 1; // past descriptor

        // Read data blocks.
        let mut data_blocks: Vec<Vec<u8>> = Vec::with_capacity(n_data as usize);
        for _ in 0..n_data {
            let db = read_ring_block(ring_pos)?;
            data_blocks.push(db);
            ring_pos += 1;
        }

        // Check for optional revoke block.
        let next_block = read_ring_block(ring_pos)?;
        let next_hdr = parse_header(&next_block);

        let mut has_revoke = false;
        if let Some(nh) = &next_hdr {
            if nh.kind == JournalBlockKind::Revoke as u8 && nh.seq == desc_seq {
                // Parse revoke entries.
                let revokes = parse_revoke_entries(&next_block, BLOCK_SIZE);
                for r in &revokes {
                    let existing = revoke_set.entry(r.fs_block).or_insert(0);
                    if r.seq > *existing {
                        *existing = r.seq;
                    }
                }
                has_revoke = true;
                ring_pos += 1;
            }
        }

        // Now expect a commit block.
        let commit_block = if has_revoke {
            read_ring_block(ring_pos)?
        } else {
            // next_block might be the commit block.
            next_block
        };
        let commit_hdr = parse_header(&commit_block);

        let is_committed = match commit_hdr {
            Some(ch) => {
                ch.kind == JournalBlockKind::Commit as u8
                    && ch.seq == desc_seq
                    && ch.tx_id == desc_tx_id
            }
            None => false,
        };

        if !is_committed {
            // Partial tx — no commit block. Discard and stop.
            log!(
                "efs journal: partial tx seq={} (no commit block), stopping replay scan",
                desc_seq
            );
            break;
        }

        // Verify commit CRC.
        let commit_crc = {
            let off = core::mem::size_of::<JournalBlockHeader>();
            u32::from_le_bytes([
                commit_block[off],
                commit_block[off + 1],
                commit_block[off + 2],
                commit_block[off + 3],
            ])
        };
        let mut payload: Vec<u8> = Vec::with_capacity(n_data as usize * BLOCK_SIZE);
        for db in &data_blocks {
            payload.extend_from_slice(db);
        }
        let expected_crc = commit_block_checksum(&payload);
        if commit_crc != expected_crc {
            log!(
                "efs journal: tx seq={} commit CRC mismatch (got {:#x}, expected {:#x}), stopping",
                desc_seq,
                commit_crc,
                expected_crc
            );
            break;
        }

        ring_pos += 1; // past commit block
        let tx_blocks = 1 + n_data + if has_revoke { 1 } else { 0 } + 1;
        total_ring_blocks += tx_blocks;
        expected_seq += 1;
        live_end_pos = ring_pos;

        committed_txs.push(CommittedTx {
            seq: desc_seq,
            entries,
            data_blocks,
        });
    }

    if committed_txs.is_empty() {
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

    'outer: for tx in &committed_txs {
        for (i, entry) in tx.entries.iter().enumerate() {
            let fs_block = entry.fs_block;

            // Check revoke set: skip if this block was revoked at a seq >= this tx's seq.
            if let Some(&revoke_seq) = revoke_set.get(&fs_block) {
                if revoke_seq >= tx.seq {
                    continue;
                }
            }

            let mut data = tx.data_blocks[i].clone();

            // Un-escape if the descriptor entry was marked ESCAPED.
            if entry.flags & DESC_FLAG_ESCAPED != 0 {
                data[..4].copy_from_slice(&JOURNAL_BLOCK_MAGIC.to_le_bytes());
            }

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
        total_ring_blocks
    );

    Ok(ReplayResult {
        txs_applied: applied,
        next_seq: expected_seq,
        next_block: live_end_pos,
    })
}

// ---- Internal helpers -------------------------------------------------------

struct CommittedTx {
    seq: u64,
    entries: Vec<efs_common::DescriptorEntry>,
    data_blocks: Vec<Vec<u8>>,
}
