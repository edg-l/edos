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

fn block_write(device_id: u64, lba: u64, sectors: u16, buf: &[u8]) -> Result<(), AhciError> {
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    let h = dev.submit_write(
        lba,
        sectors as u32,
        BlockBuffer::Slice {
            ptr: buf.as_ptr() as *mut u8,
            len: buf.len(),
        },
        WriteFlags::NONE,
    )?;
    h.wait()?;
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

/// Result of replay: how many transactions were applied. The ring cursors are
/// not reported back — the journal superblock carries `head_block`, and that is
/// what the caller resets `tail_block` to once replay has applied everything.
pub struct ReplayResult {
    pub txs_applied: u64,
}

/// Replay committed journal transactions to their home FS locations.
///
/// `device_id`: AHCI device.
/// `first_block`: absolute EFS block number of the journal region start.
/// `block_count`: total journal blocks (including the JSB at block 0).
/// `partition_start_lba`: starting LBA of the EFS partition (for fs_block → LBA).
/// `head_seq`, `tail_seq`: from the validated JournalSuperblock.
///
/// Returns the number of transactions replayed and ring blocks consumed.
/// On a clean journal (head == tail), returns 0 immediately.
pub fn replay(
    device_id: u64,
    first_block: u64,
    block_count: u32,
    partition_start_lba: u64,
    head_seq: u64,
    tail_seq: u64,
    tail_block: u64,
) -> Result<ReplayResult, AhciError> {
    if head_seq == tail_seq {
        log!("efs journal: clean, no replay needed");
        return Ok(ReplayResult { txs_applied: 0 });
    }

    log!(
        "efs journal: replay start tail_seq={} head_seq={}",
        tail_seq,
        head_seq
    );

    let ring_size = block_count as u64 - 1; // block 0 = JSB

    // Read one journal ring block by its ring-internal index.
    let read_ring_block = |ring_idx: u64| -> Result<Vec<u8>, AhciError> {
        let wrapped = (ring_idx % ring_size) + 1;
        let lba = (first_block + wrapped) * SECTORS_PER_BLOCK as u64;
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

    loop {
        if ring_pos >= tail_block + ring_size {
            // Don't scan more than one full ring from the starting position.
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

        committed_txs.push(CommittedTx {
            seq: desc_seq,
            entries,
            data_blocks,
        });
    }

    if committed_txs.is_empty() {
        log!("efs journal: no committed transactions to replay");
        // Still reset JSB tail = head so next mount is clean.
        return Ok(ReplayResult { txs_applied: 0 });
    }

    // ---- Pass 2: apply committed txs ----------------------------------------
    let mut applied = 0u64;

    for tx in &committed_txs {
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

            // Write to home location via direct AHCI (not through the block page
            // cache — the cache isn't populated yet during early mount).
            let lba = partition_start_lba + fs_block * SECTORS_PER_BLOCK as u64;
            block_write(device_id, lba, SECTORS_PER_BLOCK, &data)?;
        }
        applied += 1;
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
    })
}

// ---- Internal helpers -------------------------------------------------------

struct CommittedTx {
    seq: u64,
    entries: Vec<efs_common::DescriptorEntry>,
    data_blocks: Vec<Vec<u8>>,
}
