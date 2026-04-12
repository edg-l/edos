// EFS write-ahead journal: ring I/O and bookkeeping.
//
// Phase 3 implements: Journal struct, ring-block I/O helpers, write_journal_sb,
// and a seal_and_commit stub with bookkeeping only (no block writes yet).
//
// Phase 4 adds: enrolled_blocks / revokes to Transaction, TxHandle RAII,
// Journal::begin_tx, and real seal_and_commit I/O body.
// Phase 5 adds: committer kthread, writeback gating, advance_tail,
// force_commit_and_wait, and sys_sync / sys_fsync wiring.

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};

use efs_common::{
    DescriptorEntry, JOURNAL_BLOCK_MAGIC, JOURNAL_MAGIC, JournalBlockHeader, JournalBlockKind,
    JournalSuperblock, RevokeEntry, commit_block_checksum, journal_sb_checksum,
};

use crate::{
    drivers::ahci::{AhciError, direct},
    thread::{mutex::BlockingMutex, waitqueue::WaitQueue},
};

use super::block_page_cache::CachedBlockPage;

pub mod committer;
pub mod tx;

// ---- Constants ---------------------------------------------------------------

/// 512-byte sectors per 4 KiB journal block.
const SECTORS_PER_BLOCK: u16 = 8;

/// Block size in bytes (must match the filesystem block size).
const BLOCK_SIZE: usize = 4096;

// ---- Transaction ------------------------------------------------------------

/// A single in-flight transaction.
pub struct Transaction {
    /// Sequence number assigned to this transaction.
    pub seq: u64,
    /// Unique transaction identifier (used in block headers).
    pub tx_id: u64,
    /// Metadata pages enrolled in this transaction: (device_id, fs_block) -> page arc.
    pub enrolled_blocks: BTreeMap<(u64, u64), Arc<CachedBlockPage>>,
    /// Blocks to be revoked: (device_id, fs_block).
    pub revokes: BTreeSet<(u64, u64)>,
}

impl Transaction {
    pub fn new(seq: u64, tx_id: u64) -> Self {
        Self {
            seq,
            tx_id,
            enrolled_blocks: BTreeMap::new(),
            revokes: BTreeSet::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.enrolled_blocks.is_empty() && self.revokes.is_empty()
    }
}

// ---- JournalState -----------------------------------------------------------

pub(crate) struct JournalState {
    /// Sequence number of the next transaction to be created.
    pub head_seq: u64,
    /// Oldest sequence number that has not yet been checkpointed to disk.
    pub tail_seq: u64,
    /// The currently open (accumulating) transaction.
    pub active: Transaction,
    /// Transactions that have been sealed but not yet committed to disk.
    pub sealed: VecDeque<Transaction>,
    /// Highest sequence number that has been written to disk (committed).
    pub committed_seq: u64,
}

// ---- Journal ----------------------------------------------------------------

pub struct Journal {
    pub device_id: u64,
    first_block: u64,
    block_count: u32,
    pub(crate) state: BlockingMutex<JournalState>,
    /// Woken whenever a transaction is committed.
    pub commit_wq: WaitQueue,
    /// Woken when the committer should process immediately (e.g. force_commit).
    pub commit_kick_wq: WaitQueue,
    /// Maps (device_id, fs_block) -> seq of the tx that last enrolled this block.
    /// Used by writeback to skip journalled blocks until their tx is committed.
    pub checkpoint_tracker: BlockingMutex<BTreeMap<(u64, u64), u64>>,
    /// Monotonically increasing tx_id counter.
    tx_id_counter: AtomicU64,
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
                active: Transaction::new(head_seq, head_seq),
                sealed: VecDeque::new(),
                committed_seq: head_seq.saturating_sub(1),
            }),
            commit_wq: WaitQueue::new(),
            commit_kick_wq: WaitQueue::new(),
            checkpoint_tracker: BlockingMutex::new(BTreeMap::new()),
            tx_id_counter: AtomicU64::new(head_seq),
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

    // ---- TxHandle API -------------------------------------------------------

    /// Allocate a fresh transaction handle for the caller to enroll blocks into.
    ///
    /// The handle's Drop merges enrolled blocks into the active transaction.
    /// Interrupts must be enabled when calling this (asserted internally).
    pub fn begin_tx(self: &Arc<Self>) -> tx::TxHandle<'_> {
        debug_assert!(
            x86_64::instructions::interrupts::are_enabled(),
            "begin_tx called with interrupts disabled"
        );
        tx::TxHandle::new(self)
    }

    /// Allocate a fresh unique tx_id.
    pub(crate) fn next_tx_id(&self) -> u64 {
        self.tx_id_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    // ---- Committed seq accessor --------------------------------------------

    pub fn committed_seq(&self) -> u64 {
        self.state.lock().committed_seq
    }

    // ---- Writeback gating --------------------------------------------------

    /// Returns true if the page at `(dev, block)` may be flushed to its home
    /// location.  A page must not be flushed until the tx that enrolled it has
    /// been committed (its journal copy is safe for replay before home copy).
    #[allow(dead_code)]
    pub fn is_safe_to_flush(&self, dev: u64, block: u64) -> bool {
        let tracker = self.checkpoint_tracker.lock();
        match tracker.get(&(dev, block)) {
            Some(&seq) => seq <= self.committed_seq(),
            None => true,
        }
    }

    /// Called by writeback after a page has been successfully flushed to its
    /// home location.  Removes the tracker entry only if the enrolled seq still
    /// matches (a later tx may have re-enrolled the block with a higher seq).
    pub fn note_checkpointed(&self, dev: u64, block: u64, expected_seq: u64) {
        let mut tracker = self.checkpoint_tracker.lock();
        if tracker.get(&(dev, block)).copied() == Some(expected_seq) {
            tracker.remove(&(dev, block));
        }
        // If a later tx re-enrolled this block, the map entry has a higher seq.
        // Leave it alone so it gets flushed after the later commit.
    }

    // ---- has_pending_work ---------------------------------------------------

    /// True if there is anything for the committer to process.
    pub fn has_pending_work(&self) -> bool {
        let s = self.state.lock();
        !s.active.is_empty() || !s.sealed.is_empty()
    }

    // ---- kick_committer -----------------------------------------------------

    pub fn kick_committer(&self) {
        self.commit_kick_wq.wake_all();
    }

    // ---- advance_tail -------------------------------------------------------

    /// Advance the journal tail past transactions that have been both committed
    /// and checkpointed (all enrolled blocks flushed to their home locations).
    /// Persists the new tail to the journal superblock.
    pub fn advance_tail(&self) -> Result<(), AhciError> {
        let min_journaled_seq = {
            let tracker = self.checkpoint_tracker.lock();
            let committed = self.committed_seq();
            tracker.values().copied().min().unwrap_or(committed)
        };

        {
            let mut state = self.state.lock();
            // Remove sealed txs that are fully checkpointed.
            while let Some(front) = state.sealed.front() {
                if front.seq >= min_journaled_seq {
                    break;
                }
                state.sealed.pop_front();
            }
            let new_tail = state
                .sealed
                .front()
                .map(|tx| tx.seq)
                .unwrap_or(state.head_seq);
            if new_tail > state.tail_seq {
                state.tail_seq = new_tail;
            }
        }

        self.write_journal_sb()
    }

    // ---- force_commit_and_wait ----------------------------------------------

    /// Seal the active transaction (if non-empty) and block until it is fully
    /// committed to the journal ring.  Used by sys_sync and sys_fsync.
    pub fn force_commit_and_wait(&self) -> Result<(), AhciError> {
        // Snapshot the seq we need committed before we kick.
        let target_seq = {
            let state = self.state.lock();
            // The active transaction's seq will become committed after a seal.
            // If active is empty and head_seq-1 is already committed, we are done.
            state.active.seq
        };

        self.kick_committer();

        // Wait until committed_seq >= target_seq.
        self.commit_wq
            .wait_until(|| self.committed_seq() >= target_seq);
        Ok(())
    }

    // ---- Seal and commit ----------------------------------------------------

    /// Seal the active transaction, drain sealed queue to disk, then bump
    /// `committed_seq` and wake `commit_wq`.
    ///
    /// This may be called from the committer kthread or synchronously from
    /// `force_commit_and_wait`.  I/O is performed without holding the state lock.
    pub fn seal_and_commit(&self) -> Result<(), AhciError> {
        // Step 1: move active tx to sealed queue if non-empty.
        {
            let mut s = self.state.lock();
            if !s.active.is_empty() {
                let next_seq = s.head_seq + 1;
                let next_tx_id = self.next_tx_id();
                let new_active = Transaction::new(next_seq, next_tx_id);
                let tx = core::mem::replace(&mut s.active, new_active);
                s.sealed.push_back(tx);
                s.head_seq = next_seq;
            }
            if s.sealed.is_empty() {
                return Ok(());
            }
        }

        // Step 2: drain sealed queue.  We pop one tx at a time from the front.
        // We do NOT hold the state lock during I/O so other threads can enroll.
        loop {
            let tx = {
                let mut s = self.state.lock();
                match s.sealed.pop_front() {
                    Some(tx) => tx,
                    None => break,
                }
            };

            if tx.is_empty() {
                // Nothing to write; just bump committed_seq.
                let mut s = self.state.lock();
                s.committed_seq = tx.seq;
                drop(s);
                self.commit_wq.wake_all();
                continue;
            }

            // Build descriptor entries.
            let entries: Vec<DescriptorEntry> = tx
                .enrolled_blocks
                .keys()
                .map(|&(_dev, fs_block)| DescriptorEntry { fs_block })
                .collect();

            // Build revoke entries (use the tx seq as the revoke seq).
            let revoke_entries: Vec<RevokeEntry> = tx
                .revokes
                .iter()
                .map(|&(_dev, fs_block)| RevokeEntry {
                    fs_block,
                    seq: tx.seq,
                })
                .collect();

            // Calculate how many ring slots we need:
            //   1 descriptor + N data blocks + (1 revoke if any) + 1 commit
            let n_data = entries.len() as u64;
            let n_revoke = if revoke_entries.is_empty() {
                0u64
            } else {
                1u64
            };
            let needed = 1 + n_data + n_revoke + 1;

            // Check ring capacity (tail_seq to head_seq = used slots).
            let available = {
                let s = self.state.lock();
                let ring_size = self.block_count as u64 - 1; // block 0 is JSB
                let used = s.head_seq.saturating_sub(s.tail_seq);
                ring_size.saturating_sub(used)
            };

            if needed > available {
                // Ring full; re-push tx and return error.
                let mut s = self.state.lock();
                s.sealed.push_front(tx);
                return Err(AhciError::IoError);
            }

            // Compute ring write position: use (tail_seq-based) absolute index.
            // block_pos 0 -> ring slot 1 (slot 0 is the JSB), wraps modulo (block_count-1).
            let ring_base = {
                let s = self.state.lock();
                // The next write position in the ring is derived from head_seq.
                // We track it as: position = head_seq (monotonically increasing),
                // ring_slot = (position % ring_size) + 1.
                // We start writing at the current head_seq - 1 (the just-sealed tx's seq).
                s.head_seq - 1 // The sealed tx we just popped had this seq before bump
            };
            // The ring index for the first block of this tx.
            let ring_start = tx.seq; // Each tx writes starting at its seq-based offset.

            let ring_size = self.block_count as u64 - 1;
            let mut ring_pos = ring_start;

            // Write descriptor block.
            let desc_block = self.build_descriptor_block(tx.seq, tx.tx_id, &entries);
            let desc_ring_idx = ring_pos % ring_size;
            self.write_journal_block(desc_ring_idx, &desc_block)?;
            ring_pos += 1;

            // Write data blocks (one per enrolled page).
            let mut payload_bytes: Vec<u8> = Vec::with_capacity(n_data as usize * BLOCK_SIZE);
            for (_key, page) in &tx.enrolled_blocks {
                // SAFETY: no writer holds write_lock here; we are reading a consistent snapshot.
                let page_data = unsafe { page.as_slice() };
                let data_ring_idx = ring_pos % ring_size;
                self.write_journal_block(data_ring_idx, page_data)?;
                payload_bytes.extend_from_slice(page_data);
                ring_pos += 1;
            }

            // Write revoke block if needed.
            if !revoke_entries.is_empty() {
                let revoke_block = self.build_revoke_block(tx.seq, tx.tx_id, &revoke_entries);
                let revoke_ring_idx = ring_pos % ring_size;
                self.write_journal_block(revoke_ring_idx, &revoke_block)?;
                ring_pos += 1;
            }

            // Ordering barrier: flush drive write cache before commit block.
            direct::flush_cache(self.device_id)?;

            // Compute CRC over all payload bytes.
            let payload_crc = commit_block_checksum(&payload_bytes);

            // Write commit block with FUA.
            let commit_block = self.build_commit_block(tx.seq, tx.tx_id, payload_crc);
            let commit_ring_idx = ring_pos % ring_size;
            self.write_journal_block_fua(commit_ring_idx, &commit_block)?;

            let _ = ring_base; // used for documentation; ring_start carries the actual position

            // Success: bump committed_seq and wake waiters.
            {
                let mut s = self.state.lock();
                s.committed_seq = tx.seq;
            }
            self.commit_wq.wake_all();
        }

        Ok(())
    }

    /// Commit only if there is pending work; used by the committer kthread.
    pub fn seal_and_commit_if_needed(&self) -> Result<(), AhciError> {
        if self.has_pending_work() {
            self.seal_and_commit()
        } else {
            Ok(())
        }
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
