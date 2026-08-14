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
    DESC_FLAG_ESCAPED, DescriptorEntry, JOURNAL_BLOCK_MAGIC, JOURNAL_MAGIC, JournalSuperblock,
    RevokeEntry, build_commit_block, build_descriptor_block, build_revoke_block,
    commit_block_checksum, journal_sb_checksum,
};

use crate::{
    debug::lock_order::{RANK_JOURNAL_STATE, RANK_JOURNAL_TRACKER},
    drivers::{
        ahci::AhciError,
        block_io::{self, BlockBuffer, BlockIoHandle, WriteFlags},
    },
    ranked_lock,
    thread::{mutex::BlockingMutex, waitqueue::WaitQueue},
};

/// Issue a sector-level write and return its handle without waiting, so the
/// caller can keep further commands outstanding behind it. `buf` is the DMA
/// source and must outlive the handle.
fn submit_block_write(
    device_id: u64,
    lba: u64,
    sectors: u16,
    buf: &[u8],
) -> Result<Arc<BlockIoHandle>, AhciError> {
    let dev = block_io::lookup(device_id).ok_or(AhciError::InvalidDevice)?;
    dev.submit_write(
        lba,
        sectors as u32,
        BlockBuffer::Slice {
            ptr: buf.as_ptr() as *mut u8,
            len: buf.len(),
        },
        WriteFlags::NONE,
    )
    .map_err(Into::into)
}

/// Ring commands a transaction has issued but not waited on yet.
///
/// A transaction's descriptor, data and revoke blocks carry no ordering
/// requirement among themselves: what the format requires is that all of them
/// are on the platter before the commit block, which the flush barrier and the
/// FUA commit provide. So they are issued back to back and waited on
/// afterwards, and the drive sees a queue rather than one command per round
/// trip.
struct RingWrites {
    device_id: u64,
    inflight: VecDeque<Arc<BlockIoHandle>>,
    failure: Option<AhciError>,
    /// Commands issued, counted so `/proc/journal_stats` can show how many
    /// ring blocks one command carries.
    commands: u64,
}

impl RingWrites {
    /// Outstanding commands allowed at once, kept under `OWNED_OPS_CAP` so
    /// every one of them still gets its cancellation hookup.
    const MAX_INFLIGHT: usize = 16;

    fn new(device_id: u64) -> Self {
        Self {
            device_id,
            inflight: VecDeque::new(),
            failure: None,
            commands: 0,
        }
    }

    /// Queue one command, first waiting for the oldest if the queue is full.
    /// Returns false once anything has failed, so a caller mid-run stops
    /// issuing rather than piling commands behind a broken one.
    fn submit(&mut self, lba: u64, sectors: u16, buf: &[u8]) -> bool {
        while self.inflight.len() >= Self::MAX_INFLIGHT {
            let Some(done) = self.inflight.pop_front() else {
                break;
            };
            if let Err(e) = done.wait() {
                self.failure.get_or_insert(e.into());
            }
        }
        match submit_block_write(self.device_id, lba, sectors, buf) {
            Ok(handle) => {
                self.commands += 1;
                self.inflight.push_back(handle);
                self.failure.is_none()
            }
            Err(e) => {
                self.failure.get_or_insert(e);
                false
            }
        }
    }

    /// Wait for every outstanding command. This must run before the buffers
    /// they read from are dropped, on the failure path too: a command that
    /// reported an error may still have DMA in flight against its source.
    fn drain(&mut self) -> Result<(), AhciError> {
        while let Some(done) = self.inflight.pop_front() {
            if let Err(e) = done.wait() {
                self.failure.get_or_insert(e.into());
            }
        }
        match self.failure.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
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
#[cfg(feature = "fault-inject")]
pub mod faultinject;
pub mod replay;
pub mod tx;

// ---- Commit counters ---------------------------------------------------------

// Reported by `/proc/journal_stats`. A commit is three separately timed steps
// against the drive -- the queued ring batch, the cache-flush barrier that
// orders it, and the FUA commit block -- and only the first of them scales
// with the size of the transaction, so they are counted apart. `commands`
// against `ring_blocks` is what shows whether the batch is being coalesced.

/// Transactions written to the ring.
pub static JOURNAL_COMMITS: AtomicU64 = AtomicU64::new(0);
/// Sealed transactions that carried nothing and only bumped the sequence.
pub static JOURNAL_EMPTY_COMMITS: AtomicU64 = AtomicU64::new(0);
/// Ring blocks written: descriptor + data + revoke + commit.
pub static JOURNAL_RING_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Metadata blocks carried, i.e. ring blocks less the per-transaction overhead.
pub static JOURNAL_DATA_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Device commands the ring batches took, excluding the FUA commit block.
pub static JOURNAL_COMMANDS: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent issuing and draining the ring batch.
pub static JOURNAL_RING_US: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent in the ordering flush between the batch and the commit.
pub static JOURNAL_FLUSH_US: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent writing commit blocks with FUA.
pub static JOURNAL_COMMIT_US: AtomicU64 = AtomicU64::new(0);
/// Checkpoint passes a commit had to run because the ring was full.
pub static JOURNAL_CHECKPOINTS: AtomicU64 = AtomicU64::new(0);

/// Whole microseconds between `t0` and now.
fn us_since(t0: crate::timer::Instant) -> u64 {
    crate::timer::Instant::now().duration_since(t0).as_micros() as u64
}

// ---- Constants ---------------------------------------------------------------

/// 512-byte sectors per 4 KiB journal block.
const SECTORS_PER_BLOCK: u16 = 8;

/// Block size in bytes (must match the filesystem block size).
const BLOCK_SIZE: usize = 4096;
/// Ring blocks one AHCI command may carry, from the 248-entry PRDT.
const MAX_RUN_BLOCKS: u64 = 248;

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
    /// First journal block, counted from the start of the partition.
    first_block: u64,
    /// LBA the partition starts at. Journal blocks are partition-relative, so
    /// every LBA the journal computes has to add this; without it the ring and
    /// the superblock land ahead of the partition, on top of file data, and
    /// the superblock the mount reads is never the one being written.
    partition_start_lba: u64,
    block_count: u32,
    pub(crate) state: BlockingMutex<JournalState>,
    /// Lock-free mirror of [`JournalState::committed_seq`].
    ///
    /// `force_commit_and_wait` uses this as its wait-queue predicate, and a
    /// predicate runs with interrupts disabled, so it cannot take
    /// `state`. Published under the `state` lock at every write to
    /// `JournalState::committed_seq`; `commit_wq` is only woken afterwards,
    /// so a waiter that observes a wake sees the sequence that caused it.
    committed_seq_pub: AtomicU64,
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
        partition_start_lba: u64,
        first_block: u64,
        block_count: u32,
        head_seq: u64,
        tail_seq: u64,
        initial_head_block: u64,
    ) -> Arc<Journal> {
        Arc::new(Journal {
            device_id,
            partition_start_lba,
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
            committed_seq_pub: AtomicU64::new(head_seq.saturating_sub(1)),
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
        self.partition_start_lba + (self.first_block + ring_idx) * SECTORS_PER_BLOCK as u64
    }

    // ---- Block I/O ----------------------------------------------------------

    /// Queue one 4096-byte journal block at `journal_block_idx` (ring index).
    fn submit_journal_block(&self, journal_block_idx: u64, data: &[u8], q: &mut RingWrites) {
        let lba = self.journal_block_lba(journal_block_idx);
        q.submit(lba, SECTORS_PER_BLOCK, data);
    }

    /// Queue consecutive ring blocks, coalescing them into as few commands as
    /// the ring layout allows.
    ///
    /// The ring is contiguous on disk and only the committer ever writes it, so
    /// a transaction's data blocks are exactly the shape where one large
    /// command beats many small ones: a long run of adjacent blocks owned
    /// exclusively by this caller. Runs are cut where the ring wraps back to
    /// its first block, and at the most one command can carry.
    fn submit_journal_blocks(&self, start_idx: u64, data: &[u8], q: &mut RingWrites) {
        debug_assert!(data.len().is_multiple_of(BLOCK_SIZE));
        let ring_size = self.block_count as u64 - 1;
        let total = (data.len() / BLOCK_SIZE) as u64;
        let mut done = 0u64;
        while done < total {
            let idx = (start_idx + done) % ring_size;
            let until_wrap = ring_size - idx;
            let run = (total - done).min(until_wrap).min(MAX_RUN_BLOCKS);
            let off = done as usize * BLOCK_SIZE;
            let len = run as usize * BLOCK_SIZE;
            if !q.submit(
                self.journal_block_lba(idx),
                SECTORS_PER_BLOCK * run as u16,
                &data[off..off + len],
            ) {
                return;
            }
            done += run;
        }
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
        let lba = self.partition_start_lba + self.first_block * SECTORS_PER_BLOCK as u64;
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
        self.committed_seq_pub.load(Ordering::Acquire)
    }

    /// Record `seq` as committed. The caller holds the state guard, so the
    /// field and its lock-free mirror move together and cannot drift.
    fn set_committed_seq(&self, state: &mut JournalState, seq: u64) {
        state.committed_seq = seq;
        self.committed_seq_pub.store(seq, Ordering::Release);
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

    /// Drop tracker entries for blocks that are no longer dirty in the block
    /// page cache.
    ///
    /// An entry only clears when someone reports the block reaching its home
    /// location, and not every writer goes through the tracked writeback path:
    /// the journal writes its own blocks, and filesystems write file data
    /// straight to the device. A block that is not dirty anywhere is at home
    /// by definition, so holding its entry only pins the tail -- and a pinned
    /// tail eventually stops the ring, which stops commits, which loses the
    /// metadata those commits carried.
    ///
    /// Snapshot first, query second, remove third: the tracker (130) must not
    /// be held while asking the block cache (110), which ranks below it.
    fn prune_checkpointed(&self) {
        let keys: Vec<(u64, u64)> = {
            let tracker = ranked_lock!(
                RANK_JOURNAL_TRACKER,
                "Journal.checkpoint_tracker",
                self.checkpoint_tracker
            );
            tracker.keys().copied().collect()
        };
        if keys.is_empty() {
            return;
        }

        let cache = crate::fs::block_page_cache::BlockPageCache::global();
        let at_home: Vec<(u64, u64)> = keys
            .into_iter()
            .filter(|&(dev, block)| !cache.is_dirty(dev, block))
            .collect();
        if at_home.is_empty() {
            return;
        }

        let mut tracker = ranked_lock!(
            RANK_JOURNAL_TRACKER,
            "Journal.checkpoint_tracker",
            self.checkpoint_tracker
        );
        for key in at_home {
            tracker.remove(&key);
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

    /// Whether anything still has to be committed or checkpointed before the
    /// journal would replay as empty.
    ///
    /// Only committed work counts. A replay applies committed transactions and
    /// ignores the open one, and every checkpoint pass enrols fresh metadata
    /// into that open transaction, so including it here never reaches a fixed
    /// point: `sync` would loop until its cap on every call and still return
    /// with the journal reporting pending.
    pub fn needs_checkpoint(&self) -> bool {
        let s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
        !s.sealed.is_empty() || !s.committed_pending.is_empty()
    }

    /// Sealed transactions, committed-but-not-retired transactions, and blocks
    /// the checkpoint tracker is still waiting on. What [`Self::needs_checkpoint`]
    /// answers from, as numbers, so `sync` failing to converge can be read
    /// rather than inferred: a run that ends with `pending` stuck at a non-zero
    /// value while `tracked` is 0 means transactions are committed and fully
    /// checkpointed but never retired.
    pub fn depths(&self) -> (usize, usize, usize) {
        let tracked = ranked_lock!(
            RANK_JOURNAL_TRACKER,
            "Journal.checkpoint_tracker",
            self.checkpoint_tracker
        )
        .len();
        let s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
        (s.sealed.len(), s.committed_pending.len(), tracked)
    }

    /// Ring cursors: `(head_seq, tail_seq, head_block, tail_block, ring_size)`.
    ///
    /// Whether the live region wraps is the one property a recovery test has to
    /// establish before an unclean cut is worth taking, and it cannot be
    /// inferred from the depths: a wrapped ring is `head_block % ring_size <
    /// tail_block % ring_size`, which needs the cursors themselves.
    pub fn cursors(&self) -> (u64, u64, u64, u64, u64) {
        let s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
        (
            s.head_seq,
            s.tail_seq,
            s.head_block,
            s.tail_block,
            self.block_count as u64 - 1,
        )
    }

    /// [`has_pending_work`] for a wait-queue predicate, which runs with
    /// interrupts disabled and so must never block on `state`.
    ///
    /// A contended lock reads as "there is work": the committer then wakes,
    /// re-checks under the real lock, and finds nothing if that was wrong.
    /// The opposite bias would let it park through a pending transaction
    /// until the 5 s timeout.
    ///
    /// [`has_pending_work`]: Journal::has_pending_work
    pub fn has_pending_work_hint(&self) -> bool {
        match self.state.try_lock() {
            Some(s) => !s.active.is_empty() || !s.sealed.is_empty(),
            None => true,
        }
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
        self.prune_checkpointed();

        // The oldest sequence that still has a block waiting to reach its home
        // location. Everything below it is fully checkpointed and retirable.
        //
        // An empty tracker means *nothing* is waiting, so the bound is one past
        // the highest committed sequence rather than that sequence itself.
        // Using `committed` leaves the newest committed transaction pinned
        // forever, since the loop below retires strictly below the bound: it
        // stays in `committed_pending`, `needs_checkpoint` never goes false, and
        // `sync` spends all `SYNC_MAX_ROUNDS` rounds and then reports the
        // journal still pending. The next mount replays a transaction whose
        // blocks are already at home.
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
            tracker
                .values()
                .copied()
                .min()
                .unwrap_or(committed.saturating_add(1))
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
            if state.committed_pending.is_empty()
                && state.sealed.is_empty()
                && state.tail_seq < state.head_seq
            {
                state.tail_seq = state.head_seq;
                state.tail_block = state.head_block;
                changed = true;
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
        // The target is the highest sequence that will actually be committed,
        // which is not always the active one: `seal_active` leaves an empty
        // active transaction in place, so its sequence is never sealed and
        // never committed. Waiting on it when the active transaction is empty
        // and sealed ones are pending can only ever reach `active.seq - 1`,
        // and the wait below then runs to its full deadline.
        let target_seq = {
            let state = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
            if state.active.is_empty() {
                match state.sealed.back() {
                    Some(tx) => tx.seq,
                    // Nothing pending — already fully committed.
                    None => return Ok(()),
                }
            } else {
                state.active.seq
            }
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
        // A quarter of the ring: the cap is checked when a handle merges, so
        // the active transaction can overshoot it by one handle's worth, and
        // the result still has to fit alongside what the ring already holds.
        ((self.block_count as u64 - 1) / 4) as usize
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
                self.set_committed_seq(&mut s, tx.seq);
                drop(s);
                JOURNAL_EMPTY_COMMITS.fetch_add(1, Ordering::Relaxed);
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
                JOURNAL_CHECKPOINTS.fetch_add(1, Ordering::Relaxed);
                self.checkpoint_and_advance()?;
            }

            let Some(ring_pos_start) = ring_pos_start else {
                // Still no room after checkpointing: this transaction is larger
                // than the ring can ever hold. Put it back so a later, smaller
                // commit is not lost, and report it.
                let (head, tail) = {
                    let mut s2 = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
                    s2.sealed.push_front(tx);
                    (s2.head_block, s2.tail_block)
                };
                crate::log!(
                    "journal: transaction needs {} blocks; ring holds {}, head {}, tail {}",
                    needed,
                    ring_size,
                    head,
                    tail
                );
                return Err(AhciError::IoError);
            };

            let mut ring_pos = ring_pos_start;

            // Descriptor, data and revoke blocks all go out before the barrier
            // below, so they are queued together and drained as a batch.
            let mut q = RingWrites::new(self.device_id);
            let t_ring = crate::timer::Instant::now();

            let desc_block = build_descriptor_block(BLOCK_SIZE, tx.seq, tx.tx_id, &entries);
            self.submit_journal_block(ring_pos % ring_size, &desc_block, &mut q);
            ring_pos += 1;

            // Data blocks (one per enrolled page, possibly escaped). The CRC
            // below needs them contiguous anyway, so the same buffer is what
            // goes to the device.
            let mut payload_bytes: Vec<u8> = Vec::with_capacity(n_data as usize * BLOCK_SIZE);
            for block_data in &data_blocks {
                payload_bytes.extend_from_slice(block_data);
            }
            self.submit_journal_blocks(ring_pos, &payload_bytes, &mut q);
            ring_pos += n_data;

            // Held out here rather than inside the branch: the buffer is the
            // DMA source and must outlive the drain below.
            let revoke_block = (!revoke_entries.is_empty())
                .then(|| build_revoke_block(BLOCK_SIZE, tx.seq, tx.tx_id, &revoke_entries));
            if let Some(block) = &revoke_block {
                self.submit_journal_block(ring_pos % ring_size, block, &mut q);
                ring_pos += 1;
            }

            // Every command is waited on here, before the barrier that orders
            // the whole batch ahead of the commit block. The counters are
            // published before the error is propagated, so a failed commit
            // still shows the work it did.
            let drained = q.drain();
            JOURNAL_COMMANDS.fetch_add(q.commands, Ordering::Relaxed);
            JOURNAL_RING_US.fetch_add(us_since(t_ring), Ordering::Relaxed);
            drained?;

            // Ordering barrier: flush drive write cache before commit block.
            let t_flush = crate::timer::Instant::now();
            let flushed = block_flush(self.device_id);
            JOURNAL_FLUSH_US.fetch_add(us_since(t_flush), Ordering::Relaxed);
            flushed?;

            // Compute CRC over all payload bytes (escaped copies).
            let payload_crc = commit_block_checksum(&payload_bytes);

            // Write commit block with FUA.
            let commit_block = build_commit_block(BLOCK_SIZE, tx.seq, tx.tx_id, payload_crc);
            let t_commit = crate::timer::Instant::now();
            let written = self.write_journal_block_fua(ring_pos % ring_size, &commit_block);
            JOURNAL_COMMIT_US.fetch_add(us_since(t_commit), Ordering::Relaxed);
            written?;
            ring_pos += 1;

            JOURNAL_COMMITS.fetch_add(1, Ordering::Relaxed);
            JOURNAL_RING_BLOCKS.fetch_add(needed, Ordering::Relaxed);
            JOURNAL_DATA_BLOCKS.fetch_add(n_data, Ordering::Relaxed);

            // Success: advance head_block cursor and committed_seq.
            {
                let mut s = ranked_lock!(RANK_JOURNAL_STATE, "Journal.state", self.state);
                s.head_block = ring_pos;
                self.set_committed_seq(&mut s, tx.seq);
                s.committed_pending.push_back((tx.seq, needed));
            }

            // Publish the cursors before the next commit can reuse ring space.
            //
            // Recovery starts at the tail the superblock records. Leaving that
            // write to `advance_tail` alone lets the ring wrap several times
            // under a superblock still naming a long-retired transaction, and
            // replay then re-applies that ancient transaction over metadata
            // that has since moved on, rolling it backwards. The superblock
            // write is FUA, so the record on disk never lags the ring.
            self.write_journal_sb()?;

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
