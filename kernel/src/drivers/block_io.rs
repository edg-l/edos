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
//! [`CancellableOp::cancel`]: crate::thread::cancel::CancellableOp::cancel

use alloc::{collections::btree_map::BTreeMap, sync::Arc, sync::Weak, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

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
    const fn from_code(c: u32) -> Self {
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
    state: AtomicU8,
    error: AtomicU32,
    waiters: WaitQueue,
}

impl BlockIoHandle {
    pub fn pending() -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(BLOCK_IO_PENDING),
            error: AtomicU32::new(0),
            waiters: WaitQueue::new(),
        })
    }

    /// Driver-side completion. Idempotent: only the first caller (whoever
    /// wins the CAS) wakes waiters. Safe to call from any context that
    /// can drive the wait queue.
    pub fn complete(&self, result: Result<(), BlockError>) {
        let (new_state, code) = match result {
            Ok(()) => (BLOCK_IO_SUCCESS, 0),
            Err(e) => (BLOCK_IO_FAILED, e as u32),
        };
        // Publish error code before state so any waker that reads state in
        // a terminal value can trust the error field is set.
        self.error.store(code, Ordering::Release);
        if self
            .state
            .compare_exchange(
                BLOCK_IO_PENDING,
                new_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.waiters.wake_all();
        }
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
