/// Journal replay for efs-fsck.
///
/// Ports the kernel's two-pass algorithm to operate on a `Disk` (std::fs::File
/// backed) rather than raw AHCI. The algorithm is intentionally identical:
///
/// Pass 1: scan the ring from `tail_block`; collect committed transactions and
///         build the revoke set.
/// Pass 2: apply data blocks to their home FS locations, respecting revokes and
///         `DESC_FLAG_ESCAPED` un-escaping.
///
/// After pass 2 the JSB is rewritten with `tail == head` so the next invocation
/// is provably a no-op.
use std::collections::BTreeMap;
use std::io;

use efs_common::{
    DESC_FLAG_ESCAPED, EfsSuperblock, JOURNAL_BLOCK_MAGIC, JournalBlockKind, JournalSuperblock,
    commit_block_checksum, journal_sb_checksum, parse_descriptor_entries, parse_header,
    parse_revoke_entries,
};

use crate::disk::Disk;

/// Outcome of a replay run.
pub struct ReplayResult {
    pub tx_count: u32,
    pub blocks_written: u32,
}

struct CommittedTx {
    seq: u64,
    entries: Vec<efs_common::DescriptorEntry>,
    data_blocks: Vec<Vec<u8>>,
}

/// Read a journal ring block by its ring-internal index.
///
/// `first_block` is the absolute FS block of the journal region (block 0 = JSB).
/// `ring_size` is `block_count - 1` (excludes the JSB).
/// `ring_idx` is the raw (unwrapped) index from `tail_block` onwards.
fn read_ring_block(
    disk: &mut Disk,
    first_block: u64,
    ring_size: u64,
    ring_idx: u64,
) -> io::Result<Vec<u8>> {
    let wrapped = (ring_idx % ring_size) + 1; // +1 skips JSB at offset 0
    disk.read_block(first_block + wrapped)
}

/// Replay committed journal transactions onto their home FS locations.
///
/// Reads the on-disk `JournalSuperblock` at `sb.journal_first_block` to get
/// `head_seq`/`tail_seq`/`tail_block`, then runs two-pass replay, fsyncs,
/// and resets the JSB tail to equal head.
///
/// Returns a `ReplayResult` describing what was done. If the journal is already
/// clean (`tail_seq == head_seq`), returns immediately with zero counts.
pub fn replay(disk: &mut Disk, sb: &EfsSuperblock) -> io::Result<ReplayResult> {
    let first_block = sb.journal_first_block;
    let block_size = sb.block_size() as usize;

    // Read the JSB once at the start; capture head values for the post-replay reset.
    let jsb: JournalSuperblock = disk.read_struct_at(first_block, 0)?;

    let head_seq = jsb.head_seq;
    let head_block = jsb.head_block;
    let tail_seq = jsb.tail_seq;
    let tail_block = jsb.tail_block;

    if head_seq == tail_seq {
        return Ok(ReplayResult {
            tx_count: 0,
            blocks_written: 0,
        });
    }

    let ring_size = jsb.block_count as u64 - 1; // excludes JSB at ring index 0

    // ---- Pass 1: scan ring, collect committed txs and build revoke set --------

    let mut revoke_set: BTreeMap<u64, u64> = BTreeMap::new(); // fs_block → max_revoke_seq
    let mut committed_txs: Vec<CommittedTx> = Vec::new();
    let mut ring_pos: u64 = tail_block;

    loop {
        if ring_pos >= tail_block + ring_size {
            // Don't scan more than one full ring from the starting position.
            break;
        }

        let block = read_ring_block(disk, first_block, ring_size, ring_pos)?;
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

        let entries = parse_descriptor_entries(&block, block_size);
        let n_data = entries.len() as u64;
        ring_pos += 1; // past descriptor

        // Read data blocks.
        let mut data_blocks: Vec<Vec<u8>> = Vec::with_capacity(n_data as usize);
        for _ in 0..n_data {
            let db = read_ring_block(disk, first_block, ring_size, ring_pos)?;
            data_blocks.push(db);
            ring_pos += 1;
        }

        // Check for optional revoke block.
        let next_block = read_ring_block(disk, first_block, ring_size, ring_pos)?;
        let next_hdr = parse_header(&next_block);

        let mut has_revoke = false;
        if let Some(ref nh) = next_hdr {
            if nh.kind == JournalBlockKind::Revoke as u8 && nh.seq == desc_seq {
                let revokes = parse_revoke_entries(&next_block, block_size);
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
            read_ring_block(disk, first_block, ring_size, ring_pos)?
        } else {
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
            break;
        }

        // Verify commit CRC.
        let commit_crc = {
            let off = core::mem::size_of::<efs_common::JournalBlockHeader>();
            u32::from_le_bytes([
                commit_block[off],
                commit_block[off + 1],
                commit_block[off + 2],
                commit_block[off + 3],
            ])
        };
        let mut payload: Vec<u8> = Vec::with_capacity(n_data as usize * block_size);
        for db in &data_blocks {
            payload.extend_from_slice(db);
        }
        let expected_crc = commit_block_checksum(&payload);
        if commit_crc != expected_crc {
            break;
        }

        ring_pos += 1; // past commit block

        committed_txs.push(CommittedTx {
            seq: desc_seq,
            entries,
            data_blocks,
        });
    }

    // ---- Pass 2: apply committed txs to home FS locations ---------------------

    let mut blocks_written: u32 = 0;

    for tx in &committed_txs {
        for (i, entry) in tx.entries.iter().enumerate() {
            let fs_block = entry.fs_block;

            if let Some(&revoke_seq) = revoke_set.get(&fs_block) {
                if revoke_seq >= tx.seq {
                    continue;
                }
            }

            let mut data = tx.data_blocks[i].clone();

            if entry.flags & DESC_FLAG_ESCAPED != 0 {
                data[..4].copy_from_slice(&JOURNAL_BLOCK_MAGIC.to_le_bytes());
            }

            disk.write_block(fs_block, &data)?;
            blocks_written += 1;
        }
    }

    // Flush all replayed writes.
    disk.fsync()?;

    // ---- Reset JSB: tail = head so next invocation is a no-op ----------------

    let mut new_jsb = jsb;
    new_jsb.tail_seq = head_seq;
    new_jsb.tail_block = head_block;
    new_jsb.crc32 = journal_sb_checksum(&new_jsb);

    disk.write_struct_at(first_block, 0, &new_jsb)?;
    disk.fsync()?;

    Ok(ReplayResult {
        tx_count: committed_txs.len() as u32,
        blocks_written,
    })
}
