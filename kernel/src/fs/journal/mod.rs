// EFS write-ahead journal: ring I/O and bookkeeping stubs.
//
// Phase 3 implements: Journal struct, ring-block I/O helpers, write_journal_sb,
// and a seal_and_commit stub with bookkeeping only (no block writes yet).
//
// Phase 4 will add: enrolled_blocks / revokes to Transaction, and the actual
// descriptor + data + commit block writes inside seal_and_commit.
// Phase 5 will wire Journal into EfsDriver and spawn the committer kthread.

use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    vec,
    vec::Vec,
};

use efs_common::{
    DescriptorEntry, JOURNAL_BLOCK_MAGIC, JOURNAL_MAGIC, JournalBlockHeader, JournalBlockKind,
    JournalSuperblock, RevokeEntry, journal_sb_checksum,
};

use crate::{
    drivers::ahci::{AhciError, direct},
    thread::{mutex::BlockingMutex, waitqueue::WaitQueue},
};

// ---- Constants ---------------------------------------------------------------

/// 512-byte sectors per 4 KiB journal block.
const SECTORS_PER_BLOCK: u16 = 8;

/// Block size in bytes (must match the filesystem block size).
const BLOCK_SIZE: usize = 4096;

// ---- Transaction (Phase 3 stub) ---------------------------------------------

/// A single in-flight transaction.
///
// Phase 4: fill this in with `enrolled_blocks: BTreeMap<u64, Vec<u8>>` and
// `revokes: Vec<RevokeEntry>` so that seal_and_commit can write descriptor,
// data, and revoke blocks to the journal.
pub struct Transaction {
    /// Sequence number assigned to this transaction.
    pub seq: u64,
}

// ---- JournalState -----------------------------------------------------------

struct JournalState {
    /// Sequence number of the next transaction to be created.
    head_seq: u64,
    /// Oldest sequence number that has not yet been checkpointed to disk.
    tail_seq: u64,
    /// The currently open (accumulating) transaction.
    active: Transaction,
    /// Transactions that have been sealed but not yet committed to disk.
    sealed: VecDeque<Transaction>,
    /// Highest sequence number that has been written to disk (committed).
    committed_seq: u64,
}

// ---- Journal ----------------------------------------------------------------

pub struct Journal {
    device_id: u64,
    first_block: u64,
    block_count: u32,
    state: BlockingMutex<JournalState>,
    /// Woken whenever a transaction is committed.
    pub commit_wq: WaitQueue,
    /// Maps (device_id, fs_block) -> seq of the commit that covers it.
    /// Used by writeback to skip journalled blocks until their tx is committed.
    checkpoint_tracker: BlockingMutex<BTreeMap<(u64, u64), u64>>,
}

impl Journal {
    /// Create a new `Journal` wrapping the given device region.
    ///
    /// `head_seq` and `tail_seq` are taken from the `JournalSuperblock` read
    /// at mount time.
    pub fn new(
        device_id: u64,
        first_block: u64,
        block_count: u32,
        head_seq: u64,
        tail_seq: u64,
    ) -> Arc<Journal> {
        Arc::new(Journal {
            device_id,
            first_block,
            block_count,
            state: BlockingMutex::new(JournalState {
                head_seq,
                tail_seq,
                active: Transaction { seq: head_seq },
                sealed: VecDeque::new(),
                committed_seq: head_seq.saturating_sub(1),
            }),
            commit_wq: WaitQueue::new(),
            checkpoint_tracker: BlockingMutex::new(BTreeMap::new()),
        })
    }

    // ---- Ring arithmetic ----------------------------------------------------

    /// Convert an absolute journal block index (relative to `first_block`)
    /// into an LBA on the device.
    fn journal_block_lba(&self, journal_block_idx: u64) -> u64 {
        // journal_block_idx wraps around the ring, excluding block 0 (the JSB).
        let ring_size = self.block_count as u64 - 1;
        let ring_idx = (journal_block_idx % ring_size) + 1;
        (self.first_block + ring_idx) * SECTORS_PER_BLOCK as u64
    }

    // ---- Block builder helpers ----------------------------------------------

    /// Build a 4096-byte descriptor block for `seq`/`tx_id` listing `entries`.
    pub fn build_descriptor_block(
        &self,
        seq: u64,
        tx_id: u64,
        entries: &[DescriptorEntry],
    ) -> Vec<u8> {
        let mut buf = vec![0u8; BLOCK_SIZE];
        let hdr = JournalBlockHeader {
            magic: JOURNAL_BLOCK_MAGIC,
            kind: JournalBlockKind::Descriptor as u8,
            _pad: [0u8; 3],
            seq,
            tx_id,
        };
        write_struct(&mut buf, 0, &hdr);
        let entry_size = core::mem::size_of::<DescriptorEntry>();
        let header_size = core::mem::size_of::<JournalBlockHeader>();
        for (i, entry) in entries.iter().enumerate() {
            let off = header_size + i * entry_size;
            if off + entry_size > BLOCK_SIZE {
                break;
            }
            write_struct(&mut buf, off, entry);
        }
        buf
    }

    /// Build a 4096-byte revoke block for `seq`/`tx_id` listing `entries`.
    pub fn build_revoke_block(&self, seq: u64, tx_id: u64, entries: &[RevokeEntry]) -> Vec<u8> {
        let mut buf = vec![0u8; BLOCK_SIZE];
        let hdr = JournalBlockHeader {
            magic: JOURNAL_BLOCK_MAGIC,
            kind: JournalBlockKind::Revoke as u8,
            _pad: [0u8; 3],
            seq,
            tx_id,
        };
        write_struct(&mut buf, 0, &hdr);
        let entry_size = core::mem::size_of::<RevokeEntry>();
        let header_size = core::mem::size_of::<JournalBlockHeader>();
        for (i, entry) in entries.iter().enumerate() {
            let off = header_size + i * entry_size;
            if off + entry_size > BLOCK_SIZE {
                break;
            }
            write_struct(&mut buf, off, entry);
        }
        buf
    }

    /// Build a 4096-byte commit block for `seq`/`tx_id` with `payload_crc`.
    pub fn build_commit_block(&self, seq: u64, tx_id: u64, payload_crc: u32) -> Vec<u8> {
        let mut buf = vec![0u8; BLOCK_SIZE];
        let hdr = JournalBlockHeader {
            magic: JOURNAL_BLOCK_MAGIC,
            kind: JournalBlockKind::Commit as u8,
            _pad: [0u8; 3],
            seq,
            tx_id,
        };
        write_struct(&mut buf, 0, &hdr);
        // Store the payload CRC immediately after the header.
        let off = core::mem::size_of::<JournalBlockHeader>();
        buf[off..off + 4].copy_from_slice(&payload_crc.to_le_bytes());
        buf
    }

    // ---- Block I/O ----------------------------------------------------------

    /// Write one 4096-byte journal block at `journal_block_idx` (ring index).
    pub fn write_journal_block(
        &self,
        journal_block_idx: u64,
        data: &[u8],
    ) -> Result<(), AhciError> {
        let lba = self.journal_block_lba(journal_block_idx);
        direct::write_sectors(self.device_id, lba, data, SECTORS_PER_BLOCK)
    }

    /// Write one 4096-byte journal block with Force Unit Access for durability.
    /// Used for commit blocks and journal superblock updates.
    pub fn write_journal_block_fua(
        &self,
        journal_block_idx: u64,
        data: &[u8],
    ) -> Result<(), AhciError> {
        let lba = self.journal_block_lba(journal_block_idx);
        direct::write_sectors_fua(self.device_id, lba, data, SECTORS_PER_BLOCK)
    }

    // ---- Journal superblock update ------------------------------------------

    /// Rebuild the `JournalSuperblock` from current state and write it to disk
    /// with FUA so it survives power loss.
    pub fn write_journal_sb(&self) -> Result<(), AhciError> {
        let (head_seq, tail_seq) = {
            let s = self.state.lock();
            (s.head_seq, s.tail_seq)
        };

        let jsb = JournalSuperblock {
            magic: JOURNAL_MAGIC,
            version: 1,
            block_count: self.block_count,
            block_size: BLOCK_SIZE as u32,
            tail_seq,
            head_seq,
            crc32: 0,
            reserved: [0u8; 28],
        };
        let crc = journal_sb_checksum(&jsb);
        let jsb = JournalSuperblock { crc32: crc, ..jsb };

        // The journal SB lives at the very first journal block (block 0 of the
        // journal region = first_block of the partition journal extent).
        let lba = self.first_block * SECTORS_PER_BLOCK as u64;
        let mut block = vec![0u8; BLOCK_SIZE];
        write_struct(&mut block, 0, &jsb);
        direct::write_sectors_fua(self.device_id, lba, &block, SECTORS_PER_BLOCK)
    }

    // ---- Seal and commit (Phase 3 stub) -------------------------------------

    /// Seal the active transaction and move it to the sealed queue.
    ///
    /// Phase 3 performs only the bookkeeping: the active transaction is moved
    /// to `sealed`, a fresh empty active transaction is installed, head_seq is
    /// bumped, and committed_seq is updated so waiters on `commit_wq` can
    /// observe progress.
    ///
    // TODO Phase 4: write descriptor, data blocks, revokes, commit block here
    // before updating committed_seq and waking commit_wq.
    pub fn seal_and_commit(&self) -> Result<(), AhciError> {
        let sealed_seq = {
            let mut s = self.state.lock();
            let seq = s.active.seq;
            let next_seq = s.head_seq + 1;
            let tx = core::mem::replace(&mut s.active, Transaction { seq: next_seq });
            s.sealed.push_back(tx);
            s.head_seq = next_seq;
            s.committed_seq = seq;
            seq
        };
        let _ = sealed_seq;
        self.commit_wq.wake_all();
        Ok(())
    }
}

// ---- Helper: write a repr(C) struct into a byte buffer ----------------------

/// Copy the bytes of `val` into `buf` at `offset`.
///
/// # Safety
/// `T` must be `repr(C)` (or `repr(C, packed)`) with no uninitialized padding.
fn write_struct<T>(buf: &mut [u8], offset: usize, val: &T) {
    let size = core::mem::size_of::<T>();
    let bytes = unsafe { core::slice::from_raw_parts(val as *const T as *const u8, size) };
    buf[offset..offset + size].copy_from_slice(bytes);
}

// ---- Unit tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that build_descriptor_block produces a block with the correct
    /// header fields at offset 0.
    #[test]
    fn descriptor_block_header_fields() {
        let journal = Journal::new(0, 100, 256, 1, 1);
        let entries = [
            DescriptorEntry { fs_block: 42 },
            DescriptorEntry { fs_block: 99 },
        ];
        let block = journal.build_descriptor_block(7, 7, &entries);

        assert_eq!(block.len(), BLOCK_SIZE);

        // Read the magic from the first 4 bytes.
        let magic = u32::from_le_bytes(block[0..4].try_into().unwrap());
        assert_eq!(magic, JOURNAL_BLOCK_MAGIC);

        // Kind byte at offset 4.
        assert_eq!(block[4], JournalBlockKind::Descriptor as u8);

        // seq at offset 8 (after magic(4) + kind(1) + pad(3)).
        let seq = u64::from_le_bytes(block[8..16].try_into().unwrap());
        assert_eq!(seq, 7);

        // tx_id at offset 16.
        let tx_id = u64::from_le_bytes(block[16..24].try_into().unwrap());
        assert_eq!(tx_id, 7);

        // First DescriptorEntry fs_block at offset 24.
        let fs_block = u64::from_le_bytes(block[24..32].try_into().unwrap());
        assert_eq!(fs_block, 42);
    }
}
