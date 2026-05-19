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
//!   success. Lives in `slot_waiters[slot]` AND in the submitter's
//!   `owned_ops`; whichever path wins the state CAS owns hardware cleanup.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU8, Ordering};

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

    fn id(&self) -> (&'static str, u64) {
        ("ahci", self.slot as u64)
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
    pub port: Weak<AhciPort>,
    pub slot: usize,
    pub submitter: Weak<Thread>,
    pub state: AtomicU8,
    pub start_gen: u32,
    pub handle: Arc<BlockIoHandle>,
    pub buffer: BlockBuffer,
    pub completion: SlotCompletion,
}

impl AhciNcqOp {
    pub fn new(
        port: Weak<AhciPort>,
        slot: usize,
        submitter: Weak<Thread>,
        start_gen: u32,
        handle: Arc<BlockIoHandle>,
        buffer: BlockBuffer,
        completion: SlotCompletion,
    ) -> Self {
        Self {
            port,
            slot,
            submitter,
            state: AtomicU8::new(SLOT_PENDING),
            start_gen,
            handle,
            buffer,
            completion,
        }
    }
}

impl CancellableOp for AhciNcqOp {
    fn cancel(&self) {
        // CAS Pending -> Cancelled. If we win, publish Cancelled on the
        // handle and release the hardware slot.
        match self.state.compare_exchange(
            SLOT_PENDING,
            SLOT_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.handle.complete(Err(BlockError::Cancelled));
                if let Some(port) = self.port.upgrade() {
                    port.release_orphaned_ncq_slot(self.slot);
                }
            }
            Err(SLOT_COMPLETED) | Err(SLOT_CANCELLED) => {}
            Err(_) => unreachable!("unexpected AhciNcqOp state"),
        }
    }

    fn id(&self) -> (&'static str, u64) {
        ("ahci-ncq", self.slot as u64)
    }
}
