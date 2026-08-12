//! Journal ring scan: the read half of write-ahead recovery (see `doc/efs.md`
//! §14).
//!
//! Shared by the kernel's mount-time replay and `efs-fsck`, which differ only
//! in how a block is read and written. Both have to agree on where the live
//! region ends. A checker that finds fewer committed transactions than the
//! kernel would reports the metadata those transactions carry as corruption,
//! and offers to "repair" it by freeing blocks and clearing inodes the journal
//! was about to legitimise; one that finds more replays retired transactions
//! over metadata that has since moved on, rolling the filesystem backwards.
//!
//! The scan is bounded by sequence continuity, never by the journal
//! superblock's `head_seq`/`head_block`. Those cursors are advisory: they are
//! published after a commit and after the tail advances, so a crash in either
//! window leaves them naming a position older than what the ring holds.
//! Sequence numbers are global and never reused, so walking forward from the
//! tail and stopping at the first break finds exactly the live region whatever
//! the superblock recorded.

extern crate alloc;

use alloc::{collections::BTreeMap, vec::Vec};

use crate::{
    DESC_FLAG_ESCAPED, DescriptorEntry, JOURNAL_BLOCK_MAGIC, JournalBlockHeader, JournalBlockKind,
    commit_block_checksum, parse_descriptor_entries, parse_header, parse_revoke_entries,
};

/// One committed transaction found in the ring, with the data blocks that
/// follow its descriptor.
pub struct ScannedTx {
    /// Sequence number from the descriptor and commit headers.
    pub seq: u64,
    /// Descriptor entries, one per data block, in the same order.
    pub entries: Vec<DescriptorEntry>,
    /// Journalled block contents, still escaped where the descriptor says so.
    pub data_blocks: Vec<Vec<u8>>,
}

/// Why the scan stopped.
///
/// Every variant except `DegenerateRing` and `RingBound` is an ordinary end of
/// the live region: an unclean shutdown leaves exactly one of them behind, and
/// none of them is a reason to distrust the transactions already collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStop {
    /// `block_count` was 0, so the ring has no data blocks at all. Nothing can
    /// be read and nothing can be replayed.
    DegenerateRing,
    /// The block at the scan position is not a journal block.
    NotJournalBlock,
    /// A journal block, but not the descriptor a transaction has to start with.
    NotDescriptor { kind: u8 },
    /// A descriptor carrying some other sequence number: either the stale far
    /// side of a wrapped ring or the end of the live region.
    SequenceBreak { expected: u64, found: u64 },
    /// A descriptor with no matching commit block. The transaction was still
    /// in flight when the power went, so it is not durable and is discarded.
    PartialTx { seq: u64 },
    /// A commit block whose CRC does not match the data preceding it: a torn
    /// write, normal after a power cut.
    CommitCrcMismatch { seq: u64, found: u32, expected: u32 },
    /// A full ring's worth of blocks scanned without a continuity break, which
    /// a correctly written ring cannot produce. Suggests corruption.
    RingBound { expected: u64 },
}

/// Everything a replayer needs: the transactions to apply, the revokes that
/// veto individual blocks, and where the live region ended.
pub struct JournalScan {
    /// Committed transactions in ascending sequence order.
    pub txs: Vec<ScannedTx>,
    /// `fs_block` → highest sequence number whose journal copy is revoked.
    pub revokes: BTreeMap<u64, u64>,
    /// Sequence number the next transaction should take: one past the last
    /// committed one found, or `tail_seq` when there were none.
    ///
    /// This, and not the superblock's `head_seq`, is what a journal must be
    /// restarted on after replay. Restarting on a stale head hands out
    /// sequence numbers that are already on disk and overwrites ring blocks
    /// still holding live data.
    pub next_seq: u64,
    /// Ring cursor just past the last committed transaction found, or
    /// `tail_block` when there were none.
    pub next_block: u64,
    /// Ring blocks the committed transactions occupy, for reporting.
    pub ring_blocks: u64,
    /// Why the walk ended.
    pub stop: ScanStop,
}

impl JournalScan {
    /// Whether the ring holds committed work that has not reached its home
    /// blocks.
    ///
    /// This is the only sound test for a dirty journal. `tail_seq != head_seq`
    /// is not: `head_seq` names the open transaction, which is never
    /// committed, so a clean journal routinely sits one apart, and a crash
    /// between a commit and the superblock write leaves the two equal with a
    /// committed transaction still in the ring.
    pub fn is_dirty(&self) -> bool {
        !self.txs.is_empty()
    }
}

/// Walk the ring from `tail_block`/`tail_seq` and collect every transaction
/// that is fully committed, with the revoke set that applies to them.
///
/// `read_journal_block` is called with an offset into the journal region, where
/// 0 is the journal superblock and 1..=`ring_size` are the ring's data blocks;
/// the wrap arithmetic lives here so both replayers cannot disagree about it.
/// It must return `block_size` bytes.
///
/// `ring_size` is `block_count - 1` — the journal superblock occupies the
/// region's first block and is not part of the ring.
pub fn scan_committed<E>(
    tail_seq: u64,
    tail_block: u64,
    ring_size: u64,
    block_size: usize,
    mut read_journal_block: impl FnMut(u64) -> Result<Vec<u8>, E>,
) -> Result<JournalScan, E> {
    let mut scan = JournalScan {
        txs: Vec::new(),
        revokes: BTreeMap::new(),
        next_seq: tail_seq,
        next_block: tail_block,
        ring_blocks: 0,
        stop: ScanStop::DegenerateRing,
    };

    if ring_size == 0 {
        return Ok(scan);
    }

    let mut read_ring_block =
        |ring_idx: u64| -> Result<Vec<u8>, E> { read_journal_block((ring_idx % ring_size) + 1) };

    let mut ring_pos = tail_block;
    let mut expected_seq = tail_seq;

    loop {
        // A ring cannot hold more live transactions than it has blocks, so
        // reaching this without a continuity break means something is corrupt.
        if ring_pos >= tail_block + ring_size {
            scan.stop = ScanStop::RingBound {
                expected: expected_seq,
            };
            break;
        }

        let block = read_ring_block(ring_pos)?;
        let Some(hdr) = parse_header(&block) else {
            scan.stop = ScanStop::NotJournalBlock;
            break;
        };

        if hdr.kind != JournalBlockKind::Descriptor as u8 {
            scan.stop = ScanStop::NotDescriptor { kind: hdr.kind };
            break;
        }

        if hdr.seq != expected_seq {
            scan.stop = ScanStop::SequenceBreak {
                expected: expected_seq,
                found: hdr.seq,
            };
            break;
        }

        let desc_seq = hdr.seq;
        let desc_tx_id = hdr.tx_id;

        let entries = parse_descriptor_entries(&block, block_size);
        let n_data = entries.len() as u64;
        ring_pos += 1; // past the descriptor

        let mut data_blocks: Vec<Vec<u8>> = Vec::with_capacity(n_data as usize);
        for _ in 0..n_data {
            data_blocks.push(read_ring_block(ring_pos)?);
            ring_pos += 1;
        }

        // A revoke block is optional, so the block after the data is either
        // one or the commit block itself.
        let next_block = read_ring_block(ring_pos)?;
        let mut has_revoke = false;
        if let Some(nh) = parse_header(&next_block) {
            if nh.kind == JournalBlockKind::Revoke as u8 && nh.seq == desc_seq {
                for r in parse_revoke_entries(&next_block, block_size) {
                    let existing = scan.revokes.entry(r.fs_block).or_insert(0);
                    if r.seq > *existing {
                        *existing = r.seq;
                    }
                }
                has_revoke = true;
                ring_pos += 1;
            }
        }

        let commit_block = if has_revoke {
            read_ring_block(ring_pos)?
        } else {
            next_block
        };

        let committed = match parse_header(&commit_block) {
            Some(ch) => {
                ch.kind == JournalBlockKind::Commit as u8
                    && ch.seq == desc_seq
                    && ch.tx_id == desc_tx_id
            }
            None => false,
        };
        if !committed {
            scan.stop = ScanStop::PartialTx { seq: desc_seq };
            break;
        }

        let commit_crc = commit_payload_crc(&commit_block);
        let mut payload: Vec<u8> = Vec::with_capacity(n_data as usize * block_size);
        for db in &data_blocks {
            payload.extend_from_slice(db);
        }
        let expected_crc = commit_block_checksum(&payload);
        if commit_crc != expected_crc {
            scan.stop = ScanStop::CommitCrcMismatch {
                seq: desc_seq,
                found: commit_crc,
                expected: expected_crc,
            };
            break;
        }

        ring_pos += 1; // past the commit block

        scan.ring_blocks += 1 + n_data + if has_revoke { 1 } else { 0 } + 1;
        expected_seq += 1;
        // Advanced only past a fully accepted transaction: `ring_pos` cannot
        // serve, because a transaction rejected at its commit block has
        // already moved it past that transaction's descriptor and data.
        scan.next_seq = expected_seq;
        scan.next_block = ring_pos;
        scan.txs.push(ScannedTx {
            seq: desc_seq,
            entries,
            data_blocks,
        });
    }

    Ok(scan)
}

/// The CRC a commit block claims over the payload preceding it, stored
/// immediately after the header.
fn commit_payload_crc(commit_block: &[u8]) -> u32 {
    let off = core::mem::size_of::<JournalBlockHeader>();
    if commit_block.len() < off + 4 {
        return 0;
    }
    u32::from_le_bytes([
        commit_block[off],
        commit_block[off + 1],
        commit_block[off + 2],
        commit_block[off + 3],
    ])
}

/// The home-block write that entry `index` of `tx` calls for, as
/// `(fs_block, data)`, or `None` when a transaction at or after `tx.seq`
/// revoked that block.
///
/// `fs_block` is device-absolute (see [`DescriptorEntry::fs_block`]): it
/// already carries the partition's offset, so a replayer converts it to a
/// location with block size alone and must not add that offset again.
pub fn replay_write(
    tx: &ScannedTx,
    index: usize,
    revokes: &BTreeMap<u64, u64>,
) -> Option<(u64, Vec<u8>)> {
    let entry = tx.entries.get(index)?;
    let fs_block = entry.fs_block;

    if let Some(&revoke_seq) = revokes.get(&fs_block) {
        if revoke_seq >= tx.seq {
            return None;
        }
    }

    let mut data = tx.data_blocks.get(index)?.clone();
    if entry.flags & DESC_FLAG_ESCAPED != 0 && data.len() >= 4 {
        data[..4].copy_from_slice(&JOURNAL_BLOCK_MAGIC.to_le_bytes());
    }
    Some((fs_block, data))
}
