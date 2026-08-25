//! AHCI per-slot cancellable operations.
//!
//! Two flavors, both impl [`CancellableOp`]:
//!
//! - [`AhciSlotOp`] tracks a non-NCQ (legacy ATA, ATAPI, IDENTIFY, FLUSH)
//!   command. The submitter runs synchronously inside `with_slot_try` and
//!   completes via polling. The op only carries enough state to release the
//!   slot if the thread dies mid-I/O.
//!
//! - [`AhciNcqOp`] tracks an in-flight NCQ command in the async path. It
//!   carries an [`Arc<BlockIoHandle>`] (so the IRQ dispatcher can publish
//!   completion), the caller's [`BlockBuffer`] (pinned for the I/O), and
//!   `SlotCompletion` metadata describing any pool→buffer copy required on
//!   success. Lives in `ncq_waiters[slot]` AND in the submitter's
//!   `owned_ops`. Cancel only ever completes the handle: the drive may
//!   still be mid-DMA against the slot when the submitter dies, so hardware
//!   reclaim (the slot, and with it the `BlockBuffer`) happens only once
//!   the completion path observes the command actually finish.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use crate::{
    drivers::{
        ahci::port::AhciPort,
        block_io::{BlockBuffer, BlockError, BlockIoHandle},
    },
    thread::{cancel::CancellableOp, thread::Thread},
};

pub const SLOT_PENDING: u8 = 0;
pub const SLOT_COMPLETED: u8 = 1;
pub const SLOT_CANCELLED: u8 = 2;
/// A cancelled op whose hardware slot has been reclaimed. `SLOT_CANCELLED`
/// is not itself exclusive -- every later caller that loses the
/// `PENDING -> COMPLETED` CAS observes it -- so the reclaim transitions into
/// this state to pick exactly one winner. Reclaiming twice would decrement
/// the port's NCQ mode counter twice and clear a `ncq_waiters` entry a new
/// command had already taken.
pub const SLOT_RECLAIMED: u8 = 3;

// ---------------------------------------------------------------------------
// Legacy / non-NCQ
// ---------------------------------------------------------------------------

/// In-flight non-NCQ command tracker (legacy ATA, ATAPI, IDENTIFY, FLUSH).
///
/// The submitter runs synchronously inside `with_slot_try`; this op only
/// tracks enough state to release the slot if the thread dies during the
/// sync poll.
pub struct AhciSlotOp {
    pub port: Weak<AhciPort>,
    pub slot: usize,
    pub waiter: Weak<Thread>,
    pub state: AtomicU8,
}

impl AhciSlotOp {
    pub fn new(port: Weak<AhciPort>, slot: usize, waiter: Weak<Thread>) -> Self {
        Self {
            port,
            slot,
            waiter,
            state: AtomicU8::new(SLOT_PENDING),
        }
    }
}

impl CancellableOp for AhciSlotOp {
    fn cancel(&self) {
        match self.state.compare_exchange(
            SLOT_PENDING,
            SLOT_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                if let Some(port) = self.port.upgrade() {
                    port.release_orphaned_slot(self.slot);
                }
            }
            Err(SLOT_COMPLETED) => {
                if let Some(port) = self.port.upgrade() {
                    port.maybe_release_slot(self.slot, self);
                }
            }
            Err(SLOT_CANCELLED) => {}
            Err(_) => unreachable!("unexpected AhciSlotOp state"),
        }
    }
}

// ---------------------------------------------------------------------------
// NCQ
// ---------------------------------------------------------------------------

/// What the IRQ-side completion path must do once `SACT[slot]` clears.
pub enum SlotCompletion {
    /// Zero-copy direct DMA. Nothing to do on success.
    Direct,
    /// Pool-path read: copy `num_pages` of `slot_pool` into the destination
    /// pointer, bounded by `expected_size`.
    PoolRead {
        num_pages: usize,
        expected_size: usize,
    },
    /// Pool-path write: data was copied into the slot pool at submit time.
    /// Nothing to do on success.
    PoolWrite,
}

/// In-flight NCQ command tracker for the async submit/complete path.
pub struct AhciNcqOp {
    pub submitter: Weak<Thread>,
    pub state: AtomicU8,
    pub start_gen: u32,
    pub handle: Arc<BlockIoHandle>,
    pub buffer: BlockBuffer,
    pub completion: SlotCompletion,
    /// Set after `issue_ncq_command` writes `SACT[slot]` / `CI[slot]`.
    /// Until this flag is true, the IRQ dispatcher must skip this slot:
    /// the op is in `ncq_waiters` but the hardware command has not yet
    /// been issued, so `SACT[slot] == 0` does NOT mean "completed".
    /// Treating it as completed in that window short-circuits the wait()
    /// against a buffer the drive never wrote to.
    pub issued: AtomicBool,
    /// HPET tick captured at NCQ submit. `0` = not yet issued.
    /// Stored with `Relaxed`; the `Release` on `issued` synchronizes
    /// with the watchdog's `Acquire` load of `issued` and carries this
    /// store, so any reader that sees `issued == true` also sees a
    /// valid `issue_time`.
    pub issue_time: AtomicU64,
}

impl AhciNcqOp {
    pub fn new(
        submitter: Weak<Thread>,
        start_gen: u32,
        handle: Arc<BlockIoHandle>,
        buffer: BlockBuffer,
        completion: SlotCompletion,
    ) -> Self {
        Self {
            submitter,
            state: AtomicU8::new(SLOT_PENDING),
            start_gen,
            handle,
            buffer,
            completion,
            issued: AtomicBool::new(false),
            issue_time: AtomicU64::new(0),
        }
    }
}

impl CancellableOp for AhciNcqOp {
    fn cancel(&self) {
        // CAS Pending -> Cancelled. If we win, publish Cancelled on the
        // handle. The hardware slot is not touched here: the drive may
        // still be mid-DMA against it, and reusing it now would issue a
        // second command into a slot the drive has not finished. Reclaim
        // happens on the completion side, in `complete_ncq_slot`'s
        // `SLOT_CANCELLED` arm, once the device is actually done.
        match self.state.compare_exchange(
            SLOT_PENDING,
            SLOT_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.handle.complete(Err(BlockError::Cancelled));
            }
            Err(SLOT_COMPLETED) | Err(SLOT_CANCELLED) => {}
            Err(_) => unreachable!("unexpected AhciNcqOp state"),
        }
    }
}
