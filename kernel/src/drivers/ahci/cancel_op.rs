//! AHCI per-slot cancellable operation.
//!
//! `AhciSlotOp` is created at slot-submission time and stored in two places:
//! 1. `slot_waiters[slot]` — driver side; the interrupt/completion path reads it.
//! 2. The submitter thread's `owned_ops` registry — reaper side; `Thread::free`
//!    calls `cancel()` if the thread dies before the I/O completes.
//!
//! Exactly-once completion is guaranteed by an `AtomicU8` state machine with
//! three states: `SLOT_PENDING → SLOT_COMPLETED` (normal path) or
//! `SLOT_PENDING → SLOT_CANCELLED` (cancel path).  Whichever CAS wins owns
//! the hardware slot release.

use core::sync::atomic::{AtomicU8, Ordering};

use alloc::sync::Weak;

use crate::{
    drivers::ahci::port::AhciPort,
    thread::{cancel::CancellableOp, thread::Thread},
};

pub const SLOT_PENDING: u8 = 0;
pub const SLOT_COMPLETED: u8 = 1;
pub const SLOT_CANCELLED: u8 = 2;

/// In-flight AHCI command slot operation.
///
/// Held by both the per-slot waiter array and the submitter's `owned_ops`
/// registry.  The completion path (driver kthread) and the cancel path
/// (reaper kthread via `Thread::free`) race on `state`; whoever wins the
/// CAS owns the slot release.
pub struct AhciSlotOp {
    /// Upgraded only from the cancel path to call `release_orphaned_slot`.
    pub port: Weak<AhciPort>,
    /// Which command slot this op owns.
    pub slot: usize,
    /// The submitting thread — upgraded by the completion path to issue a wake.
    pub waiter: Weak<Thread>,
    /// State machine: 0=Pending, 1=Completed, 2=Cancelled.
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
        // CAS Pending -> Cancelled. If we win, we own hardware cleanup.
        match self.state.compare_exchange(
            SLOT_PENDING,
            SLOT_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // We own the slot. Release it so the port can reuse it.
                if let Some(port) = self.port.upgrade() {
                    port.release_orphaned_slot(self.slot);
                }
                // If port is gone, AHCI is tearing down — nothing to do.
            }
            Err(SLOT_COMPLETED) => {
                // The IRQ path won the CAS and set Completed first.
                // It then tried to wake the submitter. There is a window
                // (Running→Dying race): the IRQ saw the thread as live,
                // called wake_thread (a silent no-op per Foundation #1),
                // and did NOT call release_orphaned_slot — expecting the
                // submitter's post-park cleanup to free the slot. But since
                // we are in cancel() the thread is dying and that cleanup
                // will never run. Use maybe_release_slot to race-safely
                // reclaim the slot: if our Arc is still in slot_waiters the
                // submitter never cleared it, so we free it.
                if let Some(port) = self.port.upgrade() {
                    port.maybe_release_slot(self.slot, self);
                }
            }
            Err(SLOT_CANCELLED) => {
                // Idempotent: cancel was already called (should not happen
                // in practice since `owned_ops_cancel_all` drains the vec).
            }
            Err(_) => unreachable!("unexpected AhciSlotOp state"),
        }
    }

    fn id(&self) -> (&'static str, u64) {
        ("ahci", self.slot as u64)
    }
}
