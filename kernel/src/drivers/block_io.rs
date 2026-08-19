//! Async block-I/O trait, completion handle, and device registry.
//!
//! [`AsyncBlockDevice`] is the kernel-wide contract for block storage
//! drivers. Each `submit_*` returns an [`Arc<BlockIoHandle>`]; callers park
//! on [`BlockIoHandle::wait`] until the driver's completion path calls
//! [`BlockIoHandle::complete`]. The handle is a generic state machine; the
//! per-driver in-flight tracker (e.g. AHCI's `AhciSlotOp`) owns the
//! hardware-specific cancel path and transitions the handle to
//! `Failed(Cancelled)` when the submitter dies.
//!
//! ### Buffer lifetime contract
//!
//! A [`BlockBuffer`] points at memory a driver may DMA into or out of for as
//! long as the command is outstanding, which does not end when the handle
//! leaves `Pending`: [`CancellableOp::cancel`] completes the handle while the
//! command is still issued to the device, and there is no way to retract an
//! in-flight DMA -- the device either finishes it or the controller is
//! reset. So the bytes must stay valid until the device is actually done,
//! not until the caller stops waiting for it. `BlockBuffer` has three
//! constructors, one per way a caller can honestly make that promise:
//!
//! - [`BlockBuffer::owned_vec`] / [`BlockBuffer::owned`]: the operation
//!   co-owns the backing via an `Arc`, dropped only when the driver's
//!   in-flight tracker is dropped -- after completion or cancellation's
//!   hardware reclaim, whichever is later.
//! - [`BlockBuffer::reaped_by_submitter`]: the submitting thread promises to
//!   reap (`wait()`) the handle this buffer is submitted with before it can
//!   reach a point where it could be killed. The promise is counted on the
//!   thread and checked at `thread_exit`.
//!
//! That last one binds the driver as well as the submitter, and the
//! obligation runs the other way: **a driver that splits one request across
//! several device commands must not complete the shared handle until every
//! one of them has reported.** Completing it is what releases the submitting
//! thread, so an early completion -- on the first part to fail, say -- ends
//! the buffer's promised lifetime while the remaining commands still hold
//! descriptors into subranges of it. The right shape is to record the first
//! error and deliver it when the last part lands; `nvme::cancel_op::SplitOp`
//! is the worked example.
//!
//! [`CancellableOp::cancel`]: crate::thread::cancel::CancellableOp::cancel

use alloc::{collections::btree_map::BTreeMap, sync::Arc, sync::Weak, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use crate::thread::preempt::PreemptRwLock as RwLock;
use crate::thread::scheduler::current_thread_weak;
use crate::thread::thread::Thread;
use crate::thread::waitqueue::WaitQueue;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum BlockError {
    Io = 1,
    Timeout = 2,
    Cancelled = 3,
    InvalidArg = 4,
    DeviceGone = 5,
    NoMemory = 6,
}

impl BlockError {
    pub(crate) const fn from_code(c: u32) -> Self {
        match c {
            2 => Self::Timeout,
            3 => Self::Cancelled,
            4 => Self::InvalidArg,
            5 => Self::DeviceGone,
            6 => Self::NoMemory,
            _ => Self::Io,
        }
    }
}

// ---------------------------------------------------------------------------
// Write flags
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteFlags(u32);

impl WriteFlags {
    pub const NONE: Self = Self(0);
    pub const FUA: Self = Self(1 << 0);

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

// ---------------------------------------------------------------------------
// Buffer
// ---------------------------------------------------------------------------

/// Kernel buffer reference for block I/O. See the "Buffer lifetime contract"
/// section above for what each constructor promises.
pub struct BlockBuffer {
    ptr: *mut u8,
    len: usize,
    owner: BufferOwner,
}

enum BufferOwner {
    /// Co-owned for the whole in-flight window. The driver clones this `Arc`
    /// into its op and drops it only when the device is done, which is
    /// later than the handle leaving `Pending` whenever the submitter was
    /// cancelled. Never read back: it exists to be dropped at the right
    /// time, not to be inspected.
    #[allow(dead_code)]
    Owned(Arc<dyn Send + Sync>),
    /// The submitter promises to reap this handle before it can reach a
    /// kill point. Counted on the submitting thread and asserted at
    /// `thread_exit`.
    ReapedBySubmitter(Weak<Thread>),
    /// Outlives every thread: a `static`, a Limine module, or memory a
    /// driver owns for the device's whole lifetime.
    ///
    /// Nothing in this tree constructs this case yet -- every current
    /// caller either co-owns its backing or reaps inline -- but it is part
    /// of the type's design space and kept so the variant exists when a
    /// driver-owned buffer needs it.
    #[allow(dead_code)]
    Static,
}

unsafe impl Send for BlockBuffer {}
unsafe impl Sync for BlockBuffer {}

impl BlockBuffer {
    /// Wrap an owned `Vec`'s backing storage. No copy: the `Vec`'s heap
    /// allocation moves into the `Arc`, and the operation keeps the `Arc`
    /// alive until the device is done with it.
    pub fn owned_vec(vec: Arc<Vec<u8>>) -> Self {
        let ptr = vec.as_ptr() as *mut u8;
        let len = vec.len();
        Self {
            ptr,
            len,
            owner: BufferOwner::Owned(vec),
        }
    }

    /// Wrap a raw pointer whose backing is kept alive by `owner`.
    ///
    /// # Safety
    /// `ptr..ptr+len` must be valid for reads and writes for as long as
    /// `owner` (or a clone of it held elsewhere) is alive. The caller
    /// answers for that; this constructor does not derive `ptr` from
    /// `owner` the way [`Self::owned_vec`] does, so nothing here checks the
    /// pointer actually points into what `owner` allocated.
    pub unsafe fn owned(owner: Arc<dyn Send + Sync>, ptr: *mut u8, len: usize) -> Self {
        Self {
            ptr,
            len,
            owner: BufferOwner::Owned(owner),
        }
    }

    /// Wrap a caller's pointer on the promise that the submitting thread
    /// reaps (`wait()`s) the handle this buffer is submitted with before it
    /// can reach a point where it could be killed.
    ///
    /// # Safety
    /// `ptr..ptr+len` must be valid for reads and writes until the
    /// submitting thread reaps the handle. Breaking that promise is caught
    /// at `thread_exit` in debug builds via the thread's borrowed-DMA
    /// counter, not by this constructor.
    pub unsafe fn reaped_by_submitter(ptr: *mut u8, len: usize) -> Self {
        let weak = current_thread_weak().unwrap_or_default();
        #[cfg(debug_assertions)]
        if let Some(t) = weak.upgrade() {
            t.borrowed_dma.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            ptr,
            len,
            owner: BufferOwner::ReapedBySubmitter(weak),
        }
    }

    /// A view of `offset..offset + len` bytes of this buffer that carries
    /// the same ownership, for a driver that has to split one request into
    /// several device commands. The backing stays alive as long as any view
    /// of it does: `Owned` clones the `Arc`, and `ReapedBySubmitter` counts
    /// the extra borrow so the submitter's `thread_exit` assertion still
    /// balances.
    pub fn subrange(&self, offset: usize, len: usize) -> Self {
        assert!(
            offset.checked_add(len).is_some_and(|end| end <= self.len),
            "BlockBuffer::subrange out of range"
        );
        let owner = match &self.owner {
            BufferOwner::Owned(owner) => BufferOwner::Owned(Arc::clone(owner)),
            BufferOwner::ReapedBySubmitter(weak) => {
                #[cfg(debug_assertions)]
                if let Some(t) = weak.upgrade() {
                    t.borrowed_dma.fetch_add(1, Ordering::Relaxed);
                }
                BufferOwner::ReapedBySubmitter(weak.clone())
            }
            BufferOwner::Static => BufferOwner::Static,
        };
        Self {
            // SAFETY: bounds-checked against `self.len` above, and `self`
            // is a valid `ptr..ptr + len` range by its own constructors'
            // contract.
            ptr: unsafe { self.ptr.add(offset) },
            len,
            owner,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for BlockBuffer {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        if let BufferOwner::ReapedBySubmitter(weak) = &self.owner
            && let Some(t) = weak.upgrade()
        {
            t.borrowed_dma.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

const BLOCK_IO_PENDING: u8 = 0;
const BLOCK_IO_SUCCESS: u8 = 1;
const BLOCK_IO_FAILED: u8 = 2;

/// Completion handle returned by `submit_*`. Drivers call [`complete`] when
/// the operation finishes; callers park on [`wait`].
///
/// [`complete`]: BlockIoHandle::complete
/// [`wait`]: BlockIoHandle::wait
pub struct BlockIoHandle {
    /// Claimed before anything is published, so exactly one caller writes
    /// the result. The state alone cannot serve: a waiter reads `error`
    /// only after seeing `state` terminal, so `error` has to be published
    /// first, and a losing caller that has already written it has corrupted
    /// the winner's answer whatever the state CAS then decides.
    claimed: AtomicBool,
    state: AtomicU8,
    error: AtomicU32,
    waiters: WaitQueue,
}

impl BlockIoHandle {
    pub fn pending() -> Arc<Self> {
        Arc::new(Self {
            claimed: AtomicBool::new(false),
            state: AtomicU8::new(BLOCK_IO_PENDING),
            error: AtomicU32::new(0),
            waiters: WaitQueue::new(),
        })
    }

    /// Driver-side completion. Idempotent: only the first caller publishes
    /// a result and wakes waiters, and a later call is discarded whole
    /// rather than allowed to overwrite half of one. Safe to call from any
    /// context that can drive the wait queue.
    pub fn complete(&self, result: Result<(), BlockError>) {
        // The claim comes first because the two fields cannot be published
        // atomically together. A caller that lost would otherwise have
        // already stored its error code by the time it discovered it lost,
        // and a late `Ok` stores zero -- which a waiter that reads
        // `BLOCK_IO_FAILED` then decodes as a different, entirely plausible
        // `BlockError`.
        if self.claimed.swap(true, Ordering::AcqRel) {
            return;
        }
        let (new_state, code) = match result {
            Ok(()) => (BLOCK_IO_SUCCESS, 0),
            Err(e) => (BLOCK_IO_FAILED, e as u32),
        };
        // Publish error code before state so any waker that reads state in
        // a terminal value can trust the error field is set.
        self.error.store(code, Ordering::Release);
        self.state.store(new_state, Ordering::Release);
        self.waiters.wake_all();
    }

    /// Park until terminal. Indefinite.
    ///
    /// `WaitQueue::wait_until` may return spuriously per the kernel's park
    /// contract, so we loop on the actual state rather than trusting a
    /// single wake.
    pub fn wait(&self) -> Result<(), BlockError> {
        loop {
            match self.state.load(Ordering::Acquire) {
                BLOCK_IO_SUCCESS => return Ok(()),
                BLOCK_IO_FAILED => {
                    return Err(BlockError::from_code(self.error.load(Ordering::Acquire)));
                }
                _ => {}
            }
            self.waiters
                .wait_until(|| self.state.load(Ordering::Acquire) != BLOCK_IO_PENDING);
        }
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

pub trait AsyncBlockDevice: Send + Sync {
    /// Submit a read. `buffer.len()` must equal `sectors * sector_size`,
    /// where the device's sector size is implicit (512 for AHCI ATA today;
    /// callers in EDOS use 512-byte sectors uniformly).
    fn submit_read(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
    ) -> Result<Arc<BlockIoHandle>, BlockError>;

    /// Submit a write.
    fn submit_write(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
        flags: WriteFlags,
    ) -> Result<Arc<BlockIoHandle>, BlockError>;

    /// Submit a cache flush. May be a no-op on devices without a write cache.
    fn submit_flush(&self) -> Result<Arc<BlockIoHandle>, BlockError>;

    /// Capacity in 512-byte sectors, or 0 while the device has not been
    /// identified yet. Bounds checks for raw access through `/dev` come from
    /// here, so a driver that cannot answer must report 0 rather than guess.
    fn sector_count(&self) -> u64;

    /// Submit `N` reads at once. Default falls back to serial submits;
    /// drivers with hardware-level batching (AHCI NCQ) override.
    fn submit_read_batch(
        &self,
        reqs: Vec<(u64, u32, BlockBuffer)>,
    ) -> Result<Vec<Arc<BlockIoHandle>>, BlockError> {
        reqs.into_iter()
            .map(|(lba, sectors, buf)| self.submit_read(lba, sectors, buf))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

static DEVICES: RwLock<BTreeMap<u64, Arc<dyn AsyncBlockDevice>>> = RwLock::new(BTreeMap::new());

/// Register a block device under a stable numeric id. Called once per
/// device at driver init time.
pub fn register(device_id: u64, device: Arc<dyn AsyncBlockDevice>) {
    DEVICES.write().insert(device_id, device);
}

/// Look up a registered block device. Returns `None` if no device with
/// that id is registered.
pub fn lookup(device_id: u64) -> Option<Arc<dyn AsyncBlockDevice>> {
    DEVICES.read().get(&device_id).cloned()
}

/// Ids of every registered block device, ascending. This is what partition
/// discovery iterates, so a driver becomes scannable by registering here and
/// nothing else.
pub fn list() -> Vec<u64> {
    DEVICES.read().keys().copied().collect()
}

// ---------------------------------------------------------------------------
// Blocking helpers
// ---------------------------------------------------------------------------

/// How long an abandoned op keeps being re-issued before the error reaches
/// the caller.
///
/// A count is the wrong bound here. What abandons an op is a controller reset,
/// and a reset refuses every command issued while it runs, so a fixed handful
/// of attempts can land entirely inside one and report a failure the device
/// was about to be ready for. Ten seconds is far longer than any recovery this
/// kernel performs -- an NVMe reset takes about three milliseconds -- and far
/// shorter than a caller's patience for a device that has genuinely stopped
/// answering. It is sized for the worst case this tree can produce rather than
/// for the ordinary one: under `nvme_timeout_ms=0` the controller resets
/// hundreds of times a second, and a two-second window loses the root mount
/// about one boot in four against that.
const RETRY_WINDOW: core::time::Duration = core::time::Duration::from_secs(10);

/// Pause between attempts, so a re-issue does not spin through the couple of
/// milliseconds a reset takes.
const RETRY_BACKOFF: core::time::Duration = core::time::Duration::from_millis(5);

/// Whether an op that failed with `e` should be issued again, sleeping out the
/// backoff when it should. `started` is when the first attempt was made, and the
/// window is measured from there rather than from the last failure.
pub fn retry_after(e: BlockError, started: crate::timer::Instant) -> bool {
    if !worth_retrying(e) || started.elapsed() >= RETRY_WINDOW {
        return false;
    }
    crate::thread::scheduler::thread_sleep(RETRY_BACKOFF);
    true
}

/// True for an error that says the op was abandoned rather than answered.
///
/// Recovering a hung controller means failing every command that was in
/// flight, including ones the device was about to complete: the NVMe watchdog
/// does that on its way to a reset, and AHCI's does the same for its NCQ
/// slots. Those reads are not lost data, they are I/O that has to be asked for
/// again. An op the device rejected, one the submitter cancelled, and a device
/// that has gone away all gain nothing from a second attempt.
pub fn worth_retrying(e: BlockError) -> bool {
    matches!(e, BlockError::Io | BlockError::Timeout)
}

/// Flush the device's write cache, re-issuing an op a reset abandoned.
pub fn flush_blocking(dev: &Arc<dyn AsyncBlockDevice>) -> Result<(), BlockError> {
    let started = crate::timer::Instant::now();
    loop {
        match dev.submit_flush().and_then(|h| h.wait()) {
            Ok(()) => return Ok(()),
            Err(e) if retry_after(e, started) => {}
            Err(e) => {
                crate::log!("block: flush failed after {:?}: {e:?}", started.elapsed());
                return Err(e);
            }
        }
    }
}

/// Read into `buf` starting at `lba`, waiting for the device and re-issuing an
/// op it abandoned.
///
/// `buf.len()` must be a whole number of 512-byte sectors.
pub fn read_blocking(
    dev: &Arc<dyn AsyncBlockDevice>,
    lba: u64,
    buf: &mut [u8],
) -> Result<(), BlockError> {
    let sectors = (buf.len() / 512) as u32;
    let ptr = buf.as_mut_ptr();
    let len = buf.len();
    let started = crate::timer::Instant::now();
    loop {
        // SAFETY: the wait below reaps the op before this returns, and `buf`
        // outlives the call, which is what `reaped_by_submitter` promises.
        let buffer = unsafe { BlockBuffer::reaped_by_submitter(ptr, len) };
        match dev.submit_read(lba, sectors, buffer).and_then(|h| h.wait()) {
            Ok(()) => return Ok(()),
            Err(e) if retry_after(e, started) => {}
            Err(e) => {
                crate::log!(
                    "block: read lba={lba} sectors={sectors} failed after {:?}: {e:?}",
                    started.elapsed()
                );
                return Err(e);
            }
        }
    }
}

/// Submit several reads at once, wait for all of them, and re-issue any the
/// device abandoned.
///
/// Each request's buffer is co-owned, which is what makes a re-issue possible:
/// the bytes stay valid whatever happened to the first attempt. A failed
/// request is retried on its own rather than by resubmitting the batch, so one
/// bad run does not re-read what already landed.
pub fn read_batch_blocking(
    dev: &Arc<dyn AsyncBlockDevice>,
    reqs: &[(u64, u32, Arc<Vec<u8>>)],
) -> Result<(), BlockError> {
    let batch = reqs
        .iter()
        .map(|(lba, sectors, buf)| (*lba, *sectors, BlockBuffer::owned_vec(buf.clone())))
        .collect();
    // Every request comes back with a handle, a failed submission included, so
    // waiting on all of them is what keeps each buffer alive until its DMA is
    // finished.
    let handles = dev.submit_read_batch(batch)?;

    let started = crate::timer::Instant::now();
    let mut failed: Option<BlockError> = None;
    let mut retry: Vec<usize> = Vec::new();
    for (i, handle) in handles.iter().enumerate() {
        if let Err(e) = handle.wait() {
            if worth_retrying(e) {
                retry.push(i);
            } else {
                failed.get_or_insert(e);
            }
        }
    }
    if let Some(e) = failed {
        return Err(e);
    }

    for i in retry {
        let (lba, sectors, buf) = &reqs[i];
        loop {
            let buffer = BlockBuffer::owned_vec(buf.clone());
            match dev
                .submit_read(*lba, *sectors, buffer)
                .and_then(|h| h.wait())
            {
                Ok(()) => break,
                Err(e) if retry_after(e, started) => {}
                Err(e) => {
                    crate::log!(
                        "block: batched read lba={lba} sectors={sectors} failed after {:?}: {e:?}",
                        started.elapsed()
                    );
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Write `buf` starting at `lba`, with the same retry rule as
/// [`read_blocking`].
pub fn write_blocking(
    dev: &Arc<dyn AsyncBlockDevice>,
    lba: u64,
    buf: &[u8],
    flags: WriteFlags,
) -> Result<(), BlockError> {
    let sectors = (buf.len() / 512) as u32;
    let ptr = buf.as_ptr() as *mut u8;
    let len = buf.len();
    let started = crate::timer::Instant::now();
    loop {
        // SAFETY: as in `read_blocking`; the device only reads through this
        // pointer for a write.
        let buffer = unsafe { BlockBuffer::reaped_by_submitter(ptr, len) };
        match dev
            .submit_write(lba, sectors, buffer, flags)
            .and_then(|h| h.wait())
        {
            Ok(()) => return Ok(()),
            Err(e) if retry_after(e, started) => {}
            Err(e) => {
                crate::log!(
                    "block: write lba={lba} sectors={sectors} failed after {:?}: {e:?}",
                    started.elapsed()
                );
                return Err(e);
            }
        }
    }
}
