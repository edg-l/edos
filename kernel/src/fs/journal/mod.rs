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
    DESC_FLAG_ESCAPED, DescriptorEntry, JOURNAL_BLOCK_MAGIC, JOURNAL_MAGIC, JournalBlockHeader,
    JournalBlockKind, JournalSuperblock, RevokeEntry, commit_block_checksum, journal_sb_checksum,
};

use crate::{
    debug::lock_order::{RANK_JOURNAL_STATE, RANK_JOURNAL_TRACKER},
    drivers::{
        ahci::AhciError,
        block_io::{self, BlockBuffer, WriteFlags},
    },
    ranked_lock,
    thread::{mutex::BlockingMutex, waitqueue::WaitQueue},
};

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

fn block_write_fua(device_id: u64, lba: u64, sectors: u16, buf: &[u8]) -> Result<(), AhciError> {
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    let h = dev.submit_write(
        lba,
        sectors as u32,
        BlockBuffer::Slice {
            ptr: buf.as_ptr() as *mut u8,
            len: buf.len(),
        },
        WriteFlags::FUA,
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

use super::block_page_cache::CachedBlockPage;

pub mod committer;
pub mod replay;
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
    /// Number of ring blocks consumed when committed (set by seal_and_commit).
    pub ring_blocks: u64,
}

impl Transaction {
    pub fn new(seq: u64, tx_id: u64) -> Self {
        Self {
            seq,
            tx_id,
            enrolled_blocks: BTreeMap::new(),
            revokes: BTreeSet::new(),
            ring_blocks: 0,
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
    /// Next ring block offset to write at (counts blocks consumed, wraps mod ring_size).
    pub head_block: u64,
    /// Ring block offset of the oldest live data (advances with tail_seq).
    pub tail_block: u64,
    /// The currently open (accumulating) transaction.
    pub active: Transaction,
    /// Transactions that have been sealed but not yet committed to disk.
    pub sealed: VecDeque<Transaction>,
    /// Committed txs awaiting checkpoint (seq, ring_blocks consumed).
    pub committed_pending: VecDeque<(u64, u64)>,
    /// Highest sequence number that has been written to disk (committed).
    pub committed_seq: u64,
}

// ---- Journal ----------------------------------------------------------------

/// How many times a commit will checkpoint and re-check before giving up on
/// finding ring space. Two passes are enough when anything is checkpointable;
/// a third would only spin on a transaction that cannot fit at all.
const CHECKPOINT_ATTEMPTS: usize = 3;

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
        initial_head_block: u64,
    ) -> Arc<Journal> {
        Arc::new(Journal {
            device_id,
            first_block,
            block_count,
            state: BlockingMutex::new(JournalState {
                head_seq,
                tail_seq,
                head_block: initial_head_block,
                tail_block: initial_head_block,
                active: Transaction::new(head_seq, head_seq),
                sealed: VecDeque::new(),
                committed_pending: VecDeque::new(),
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
    /// Layout: [JournalBlockHeader (24B)] [entry_count: u32 (4B)] [DescriptorEntry * N]
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
        let header_size = core::mem::size_of::<JournalBlockHeader>();
        // Store entry count after header so replay doesn't rely on zero-termination.
        buf[header_size..header_size + 4].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        let entries_offset = header_size + 4;
        let entry_size = core::mem::size_of::<DescriptorEntry>();
        for (i, entry) in entries.iter().enumerate() {
            let off = entries_offset + i * entry_size;
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
        block_write(self.device_id, lba, SECTORS_PER_BLOCK, data)
    }

    /// Write one 4096-byte journal block with Force Unit Access for durability.
    /// Used for commit blocks and journal superblock updates.
    pub fn write_journal_block_fua(
        &self,
        journal_block_idx: u64,
        data: &[u8],
    ) -> Result<(), AhciError> {
        let lba = self.journal_block_lba(journal_block_idx);
        block_write_fua(self.device_id, lba, SECTORS_PER_BLOCK, data)
    }

    // ---- Journal superblock update ------------------------------------------

    /// Rebuild the `JournalSuperblock` from current state and write it to disk
    /// with FUA so it survives power loss.
    pub fn write_journal_sb(&self) -> Result<(), AhciError> {
        let (head_seq, tail_seq, head_block, tail_block) = {
            let s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
            (s.head_seq, s.tail_seq, s.head_block, s.tail_block)
        };

        let jsb = JournalSuperblock {
            magic: JOURNAL_MAGIC,
            version: 1,
            block_count: self.block_count,
            block_size: BLOCK_SIZE as u32,
            tail_seq,
            head_seq,
            tail_block,
            head_block,
            crc32: 0,
            reserved: [0u8; 12],
        };
        let crc = journal_sb_checksum(&jsb);
        let jsb = JournalSuperblock { crc32: crc, ..jsb };

        // The journal SB lives at the very first journal block (block 0 of the
        // journal region = first_block of the partition journal extent).
        let lba = self.first_block * SECTORS_PER_BLOCK as u64;
        let mut block = vec![0u8; BLOCK_SIZE];
        write_struct(&mut block, 0, &jsb);
        block_write_fua(self.device_id, lba, SECTORS_PER_BLOCK, &block)
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
        ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state).committed_seq
    }

    // ---- Writeback gating --------------------------------------------------

    /// Returns true if the page at `(dev, block)` may be flushed to its home
    /// location.  A page must not be flushed until the tx that enrolled it has
    /// been committed (its journal copy is safe for replay before home copy).
    #[allow(dead_code)]
    pub fn is_safe_to_flush(&self, dev: u64, block: u64) -> bool {
        // Hoist committed_seq() read before taking checkpoint_tracker to avoid
        // a tracker -> state lock-order inversion (see doc/invariants/lock-order.md,
        // Task 0.0 of Foundation #4).
        let committed = self.committed_seq();
        let tracker = ranked_lock!(
            RANK_JOURNAL_TRACKER,
            "Journal.checkpoint_tracker",
            self.checkpoint_tracker
        );
        match tracker.get(&(dev, block)) {
            Some(&seq) => seq <= committed,
            None => true,
        }
    }

    /// Sequence the block at `(dev, block)` is enrolled under, if any.
    ///
    /// Callers that wrote a block out without going through the tracked
    /// writeback path use this to report the checkpoint afterwards.
    pub fn enrolled_seq(&self, dev: u64, block: u64) -> Option<u64> {
        let tracker = ranked_lock!(
            RANK_JOURNAL_TRACKER,
            "Journal.checkpoint_tracker",
            self.checkpoint_tracker
        );
        tracker.get(&(dev, block)).copied()
    }

    /// Called by writeback after a page has been successfully flushed to its
    /// home location.  Removes the tracker entry only if the enrolled seq still
    /// matches (a later tx may have re-enrolled the block with a higher seq).
    pub fn note_checkpointed(&self, dev: u64, block: u64, expected_seq: u64) {
        let mut tracker = ranked_lock!(
            RANK_JOURNAL_TRACKER,
            "Journal.checkpoint_tracker",
            self.checkpoint_tracker
        );
        if tracker.get(&(dev, block)).copied() == Some(expected_seq) {
            tracker.remove(&(dev, block));
        }
        // If a later tx re-enrolled this block, the map entry has a higher seq.
        // Leave it alone so it gets flushed after the later commit.
    }

    // ---- has_pending_work ---------------------------------------------------

    /// True if there is anything for the committer to process.
    pub fn has_pending_work(&self) -> bool {
        let s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
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
            // Hoist committed_seq() read before taking checkpoint_tracker to avoid
            // a tracker -> state lock-order inversion (see doc/invariants/lock-order.md,
            // Task 0.0 of Foundation #4).
            let committed = self.committed_seq();
            let tracker = ranked_lock!(
                RANK_JOURNAL_TRACKER,
                "Journal.checkpoint_tracker",
                self.checkpoint_tracker
            );
            tracker.values().copied().min().unwrap_or(committed)
        };

        let mut changed = false;
        {
            let mut state = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
            // Pop committed txs that are fully checkpointed (all their enrolled
            // blocks have been flushed to home locations).
            while let Some(&(seq, blocks)) = state.committed_pending.front() {
                if seq >= min_journaled_seq {
                    break;
                }
                state.committed_pending.pop_front();
                state.tail_block += blocks;
                state.tail_seq = seq + 1;
                changed = true;
            }
            // If nothing remains, tail catches up to head.
            if state.committed_pending.is_empty() && state.sealed.is_empty() {
                if state.tail_seq < state.head_seq {
                    state.tail_seq = state.head_seq;
                    state.tail_block = state.head_block;
                    changed = true;
                }
            }
        }

        if changed {
            self.write_journal_sb()?;
        }
        Ok(())
    }

    // ---- force_commit_and_wait ----------------------------------------------

    /// Seal the active transaction (if non-empty) and block until it is fully
    /// committed to the journal ring.  Used by sys_sync and sys_fsync.
    pub fn force_commit_and_wait(&self) -> Result<(), AhciError> {
        let target_seq = {
            let state = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
            if state.active.is_empty() && state.sealed.is_empty() {
                // Nothing pending — already fully committed.
                return Ok(());
            }
            state.active.seq
        };

        self.kick_committer();

        // wait_until_timeout may return spuriously (per WaitQueue contract:
        // wake_pending tokens, thread_park_while spurious returns). Loop on
        // the actual condition with a real wall-clock deadline instead of
        // trusting a single TimedOut return.
        let deadline = crate::timer::Instant::now() + core::time::Duration::from_secs(30);
        loop {
            if self.committed_seq() >= target_seq {
                return Ok(());
            }
            let now = crate::timer::Instant::now();
            if now >= deadline {
                crate::log!(
                    "journal: force_commit_and_wait timed out waiting for seq {}",
                    target_seq
                );
                return Err(AhciError::IoError);
            }
            let remaining = deadline.duration_since(now);
            self.commit_wq
                .wait_until_timeout(|| self.committed_seq() >= target_seq, Some(remaining));
        }
    }

    // ---- Seal and commit ----------------------------------------------------

    /// Seal the active transaction, drain sealed queue to disk, then bump
    /// `committed_seq` and wake `commit_wq`.
    ///
    /// This may be called from the committer kthread or synchronously from
    /// `force_commit_and_wait`.  I/O is performed without holding the state lock.
    /// Write enrolled blocks back to their home locations and advance the tail
    /// past whatever that freed. This is the only thing that reclaims ring
    /// space, so the commit path calls it when the ring is full.
    fn checkpoint_and_advance(&self) -> Result<(), AhciError> {
        // Flush inline rather than waiting for the writeback kthread: this can
        // run *on* that thread (writeback -> filesystem flush -> journal), and
        // waiting for a pass to finish from inside one deadlocks.
        crate::fs::block_page_cache::BlockPageCache::global().flush_dirty_once(true)?;
        self.advance_tail()
    }

    /// Largest number of blocks one transaction may enroll.
    ///
    /// A sealed transaction has to fit in the ring in one piece: it is written
    /// as descriptor + data + commit before the tail can advance past it. A
    /// transaction larger than the ring can therefore never commit, and since
    /// writeback refuses to check point blocks belonging to an uncommitted
    /// transaction, the ring would never drain either. Half the ring leaves
    /// room for the descriptor, commit and revoke blocks.
    pub fn max_tx_blocks(&self) -> usize {
        ((self.block_count as u64 - 1) / 2) as usize
    }

    /// Move the active transaction to the sealed queue. Returns true if there
    /// was anything to seal. Caller holds the state lock.
    fn seal_active(&self, s: &mut JournalState) -> bool {
        if s.active.is_empty() {
            return false;
        }
        let next_seq = s.head_seq + 1;
        let next_tx_id = self.next_tx_id();
        let new_active = Transaction::new(next_seq, next_tx_id);
        let tx = core::mem::replace(&mut s.active, new_active);
        s.sealed.push_back(tx);
        s.head_seq = next_seq;
        true
    }

    /// Seal the active transaction if it has grown to [`max_tx_blocks`], so it
    /// is committed on its own rather than growing past what the ring can hold.
    ///
    /// [`max_tx_blocks`]: Self::max_tx_blocks
    pub fn seal_if_full(&self) -> bool {
        let limit = self.max_tx_blocks();
        let sealed = {
            let mut s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
            if s.active.enrolled_blocks.len() < limit {
                return false;
            }
            self.seal_active(&mut s)
        };
        if sealed {
            self.kick_committer();
        }
        sealed
    }

    pub fn seal_and_commit(&self) -> Result<(), AhciError> {
        // Step 1: move active tx to sealed queue if non-empty.
        {
            let mut s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
            self.seal_active(&mut s);
            if s.sealed.is_empty() {
                return Ok(());
            }
        }

        // Step 2: drain sealed queue.  We pop one tx at a time from the front.
        // We do NOT hold the state lock during I/O so other threads can enroll.
        loop {
            let tx = {
                let mut s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
                match s.sealed.pop_front() {
                    Some(tx) => tx,
                    None => break,
                }
            };

            if tx.is_empty() {
                // Nothing to write; just bump committed_seq.
                let mut s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
                s.committed_seq = tx.seq;
                drop(s);
                self.commit_wq.wake_all();
                continue;
            }

            // Build descriptor entries with escape detection.
            let mut entries: Vec<DescriptorEntry> = Vec::with_capacity(tx.enrolled_blocks.len());
            let mut data_blocks: Vec<Vec<u8>> = Vec::with_capacity(tx.enrolled_blocks.len());
            for (&(_dev, fs_block), page) in &tx.enrolled_blocks {
                let page_data = unsafe { page.as_slice() };
                let first_word =
                    u32::from_le_bytes([page_data[0], page_data[1], page_data[2], page_data[3]]);
                let escaped = first_word == JOURNAL_BLOCK_MAGIC;
                let mut block_copy = page_data.to_vec();
                if escaped {
                    block_copy[..4].fill(0);
                }
                entries.push(DescriptorEntry {
                    fs_block,
                    flags: if escaped { DESC_FLAG_ESCAPED } else { 0 },
                    _reserved: 0,
                });
                data_blocks.push(block_copy);
            }

            // Build revoke entries (use the tx seq as the revoke seq).
            let revoke_entries: Vec<RevokeEntry> = tx
                .revokes
                .iter()
                .map(|&(_dev, fs_block)| RevokeEntry {
                    fs_block,
                    seq: tx.seq,
                })
                .collect();

            // Calculate how many ring blocks we need:
            //   1 descriptor + N data blocks + (1 revoke if any) + 1 commit
            let n_data = data_blocks.len() as u64;
            let n_revoke = if revoke_entries.is_empty() {
                0u64
            } else {
                1u64
            };
            let needed = 1 + n_data + n_revoke + 1;

            // Check ring capacity in blocks (not tx count). A full ring is not
            // an error on its own: the space is held by committed transactions
            // whose blocks have not reached their home locations yet. Write
            // them back and advance the tail, which is what frees the ring.
            //
            // Doing this here rather than returning an error is what keeps a
            // sustained write from wedging: the committer cannot make progress
            // without space, and space cannot appear without a checkpoint.
            let ring_size = self.block_count as u64 - 1; // block 0 is JSB
            let mut ring_pos_start = None;
            for attempt in 0..CHECKPOINT_ATTEMPTS {
                let s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
                let used = s.head_block.wrapping_sub(s.tail_block);
                if needed <= ring_size.saturating_sub(used) {
                    ring_pos_start = Some(s.head_block);
                    break;
                }
                drop(s);

                if attempt + 1 == CHECKPOINT_ATTEMPTS {
                    break;
                }
                self.checkpoint_and_advance()?;
            }

            let Some(ring_pos_start) = ring_pos_start else {
                // Still no room after checkpointing: this transaction is larger
                // than the ring can ever hold. Put it back so a later, smaller
                // commit is not lost, and report it.
                let mut s2 = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
                s2.sealed.push_front(tx);
                crate::log!(
                    "journal: transaction needs {} blocks, ring holds {}",
                    needed,
                    ring_size
                );
                return Err(AhciError::IoError);
            };

            let mut ring_pos = ring_pos_start;

            // Write descriptor block.
            let desc_block = self.build_descriptor_block(tx.seq, tx.tx_id, &entries);
            self.write_journal_block(ring_pos % ring_size, &desc_block)?;
            ring_pos += 1;

            // Write data blocks (one per enrolled page, possibly escaped).
            let mut payload_bytes: Vec<u8> = Vec::with_capacity(n_data as usize * BLOCK_SIZE);
            for block_data in &data_blocks {
                self.write_journal_block(ring_pos % ring_size, block_data)?;
                payload_bytes.extend_from_slice(block_data);
                ring_pos += 1;
            }

            // Write revoke block if needed.
            if !revoke_entries.is_empty() {
                let revoke_block = self.build_revoke_block(tx.seq, tx.tx_id, &revoke_entries);
                self.write_journal_block(ring_pos % ring_size, &revoke_block)?;
                ring_pos += 1;
            }

            // Ordering barrier: flush drive write cache before commit block.
            block_flush(self.device_id)?;

            // Compute CRC over all payload bytes (escaped copies).
            let payload_crc = commit_block_checksum(&payload_bytes);

            // Write commit block with FUA.
            let commit_block = self.build_commit_block(tx.seq, tx.tx_id, payload_crc);
            self.write_journal_block_fua(ring_pos % ring_size, &commit_block)?;
            ring_pos += 1;

            // Success: advance head_block cursor and committed_seq.
            {
                let mut s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
                s.head_block = ring_pos;
                s.committed_seq = tx.seq;
                s.committed_pending.push_back((tx.seq, needed));
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
