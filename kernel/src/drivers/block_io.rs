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
//! [`BlockBuffer::Slice`] carries a raw pointer + length. The caller MUST
//! ensure the underlying allocation outlives the [`BlockIoHandle`] (i.e.
//! is still valid when `complete()` runs). The driver does not copy the
//! pointer; DMA may target it directly.

use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use spin::RwLock;

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

/// Kernel buffer reference for block I/O.
///
/// Only the `Slice` variant exists today. A future `Page` variant tied to
/// `Arc<CachedPage>` will be added when the page-fill path adopts the trait
/// (Phase C of the block-io migration).
pub enum BlockBuffer {
    /// Caller-pinned slice of kernel virtual memory.
    ///
    /// # Safety
    /// Caller MUST ensure the pointed-to bytes are valid until the handle
    /// transitions out of `Pending`.
    Slice { ptr: *mut u8, len: usize },
}

unsafe impl Send for BlockBuffer {}
unsafe impl Sync for BlockBuffer {}

impl BlockBuffer {
    pub fn len(&self) -> usize {
        match self {
            Self::Slice { len, .. } => *len,
        }
    }

    pub fn as_ptr(&self) -> *const u8 {
        match self {
            Self::Slice { ptr, .. } => *ptr,
        }
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        match self {
            Self::Slice { ptr, .. } => *ptr,
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
    pub fn wait(&self) -> Result<(), BlockError> {
        self.waiters
            .wait_until(|| self.state.load(Ordering::Acquire) != BLOCK_IO_PENDING);
        match self.state.load(Ordering::Acquire) {
            BLOCK_IO_SUCCESS => Ok(()),
            BLOCK_IO_FAILED => Err(BlockError::from_code(self.error.load(Ordering::Acquire))),
            _ => unreachable!("post-wait state must be terminal"),
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
