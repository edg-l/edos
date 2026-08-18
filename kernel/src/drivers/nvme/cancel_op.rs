//! One in-flight NVMe command: the per-command-id tracker installed in
//! `NvmeQueue.cmd_slots` and in the submitter's `owned_ops`.
//!
//! There is no way to retract an in-flight NVMe command -- the device either
//! completes it or the controller is reset (NVMe 2.0 has no reliable
//! per-command abort) -- so [`NvmeOp::cancel`] only ever completes the
//! handle. The command id, the bounce buffer and the PRP list page stay
//! reserved: NVMe 2.0 3.3.1 requires a command identifier be unique among
//! those outstanding on a queue, and reusing any of them while the device
//! may still be writing into them would let a later command's data land in
//! memory a new command already owns. Reclaiming them is the job of
//! whichever completion path (the IRQ dispatcher's `complete_command`, or
//! the watchdog) later observes the device actually finish.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::{
    drivers::{
        block_io::{BlockBuffer, BlockError, BlockIoHandle},
        dma::{DmaBuffer, dma},
        nvme::admin::NvmeController,
    },
    thread::{cancel::CancellableOp, thread::Thread},
};

pub const OP_PENDING: u8 = 0;
pub const OP_COMPLETED: u8 = 1;
pub const OP_CANCELLED: u8 = 2;
/// A cancelled op whose cid and DMA memory have been reclaimed.
/// `OP_CANCELLED` is not itself exclusive -- every later caller that loses
/// the `PENDING -> COMPLETED` CAS observes it -- so reclaiming transitions
/// into this state to pick exactly one winner. Reclaiming twice would free
/// the cid twice (handing it to two commands at once) and return the same
/// `DmaBuffer` to the allocator twice.
pub const OP_RECLAIMED: u8 = 3;

/// Which direction this command moves data, so the completion path knows
/// whether a bounced transfer needs copying back into the caller's buffer.
/// A flush moves none, and is spelled out rather than folded into `Write`
/// so that the copy-back test reads as the question it is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Read,
    Write,
    Flush,
}

/// The bounce buffer and PRP list page a command may have allocated.
/// `DmaBuffer` has no `Drop`, so whichever caller wins the reclaim
/// transition on `NvmeOp::state` must explicitly return these to `dma()`;
/// letting the last `Arc<NvmeOp>` fall strands them for the life of the
/// boot.
#[derive(Default)]
pub struct OpResources {
    pub bounce: Option<DmaBuffer>,
    pub prp_list: Option<DmaBuffer>,
}

/// Return `r`'s buffers to the DMA allocator. Idempotent in the sense that
/// an already-empty `OpResources` (the common case: a 4 KiB read fits in
/// PRP1 alone) does nothing.
pub fn dealloc_resources(r: OpResources) {
    if let Some(bounce) = r.bounce {
        let _ = dma().dealloc(bounce);
    }
    if let Some(prp_list) = r.prp_list {
        let _ = dma().dealloc(prp_list);
    }
}

/// A request too large for one command, tracked across the commands it was
/// chopped into. The caller holds one [`BlockIoHandle`]; each part is an
/// ordinary [`NvmeOp`] that reclaims its own command id, bounce buffer and
/// PRP list page, and reports here instead of completing the handle itself.
pub struct SplitOp {
    handle: Arc<BlockIoHandle>,
    remaining: AtomicU32,
    failed: AtomicBool,
}

impl SplitOp {
    pub fn new(handle: Arc<BlockIoHandle>, parts: u32) -> Self {
        Self {
            handle,
            remaining: AtomicU32::new(parts),
            failed: AtomicBool::new(false),
        }
    }

    /// Account for `parts` finished parts carrying one shared `result`.
    ///
    /// The first error completes the caller's handle at once rather than
    /// waiting for the rest: the data is already wrong, and every
    /// still-outstanding part reclaims its own resources when the device
    /// finishes with it. Success completes only when the last part lands,
    /// and only if no part failed -- `BlockIoHandle::complete` publishes
    /// its error code before the state CAS it may lose, so an `Ok` racing a
    /// recorded failure would zero the error a waiter then reads.
    pub fn parts_done(&self, parts: u32, result: Result<(), BlockError>) {
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
            self.handle.complete(result);
        }
        if self.remaining.fetch_sub(parts, Ordering::AcqRel) == parts
            && !self.failed.load(Ordering::Acquire)
        {
            self.handle.complete(Ok(()));
        }
    }
}

/// Where a finished command reports its result: to the caller's handle
/// directly, or to the split request it is one part of.
#[derive(Clone)]
pub enum Completion {
    Whole(Arc<BlockIoHandle>),
    Part(Arc<SplitOp>),
}

impl Completion {
    pub fn finish(&self, result: Result<(), BlockError>) {
        match self {
            Completion::Whole(handle) => handle.complete(result),
            Completion::Part(split) => split.parts_done(1, result),
        }
    }
}

/// In-flight tracker for one NVMe command (admin or NVM).
pub struct NvmeOp {
    #[expect(
        dead_code,
        reason = "read by the watchdog sweep to reach the controller a stale op belongs to"
    )]
    pub controller: Weak<NvmeController>,
    pub qid: u16,
    pub cid: u8,
    pub submitter: Weak<Thread>,
    pub state: AtomicU8,
    pub completion: Completion,
    pub buffer: BlockBuffer,
    pub direction: Direction,
    pub len: usize,
    /// HPET-derived nanosecond timestamp at issue, read by the watchdog's
    /// staleness sweep. `0` until the submitter stores it, immediately
    /// before ringing the doorbell.
    pub issue_time: AtomicU64,
    resources: spin::Mutex<OpResources>,
}

impl NvmeOp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        controller: Weak<NvmeController>,
        qid: u16,
        cid: u8,
        submitter: Weak<Thread>,
        completion: Completion,
        buffer: BlockBuffer,
        direction: Direction,
        len: usize,
        bounce: Option<DmaBuffer>,
        prp_list: Option<DmaBuffer>,
    ) -> Self {
        Self {
            controller,
            qid,
            cid,
            submitter,
            state: AtomicU8::new(OP_PENDING),
            completion,
            buffer,
            direction,
            len,
            issue_time: AtomicU64::new(0),
            resources: spin::Mutex::new(OpResources { bounce, prp_list }),
        }
    }

    /// Take the bounce buffer and PRP list page out for reclaim. Called
    /// exactly once per op, by whichever caller wins the terminal-state
    /// transition on `state`; the lock exists only to move a non-`Copy`
    /// value out through the shared `Arc<NvmeOp>` every completion path
    /// holds, not to protect against real contention.
    pub fn take_resources(&self) -> OpResources {
        core::mem::take(&mut *self.resources.lock())
    }
}

impl CancellableOp for NvmeOp {
    fn cancel(&self) {
        match self.state.compare_exchange(
            OP_PENDING,
            OP_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => self.completion.finish(Err(BlockError::Cancelled)),
            Err(OP_COMPLETED) | Err(OP_CANCELLED) => {}
            Err(_) => unreachable!("unexpected NvmeOp state"),
        }
    }

    fn id(&self) -> (&'static str, u64) {
        ("nvme", self.cid as u64)
    }
}
