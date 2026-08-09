//! Deferred orphan inode eviction.
//!
//! `VfsInode::drop` posts an `EvictRequest { mount_id, ino }` here instead of
//! calling `fs.evict_inode` directly.  A dedicated single-threaded kthread
//! (`evict-inode`) drains the queue and performs the potentially-blocking
//! disk-free work safely outside any reaper or driver context.
//!
//! # Design decisions
//!
//! - Queue capacity 256 (D3). Realistic orphan bursts are 0-2 per death.
//! - If the queue is full AND the caller is the reaper: park the request on an
//!   unbounded overflow list. The reaper may neither block for ring space nor
//!   evict inline, and discarding the request leaks the inode's blocks until
//!   `efs-fsck` runs.
//! - If the queue is full AND the caller is NOT the evict kthread: fall back
//!   to synchronous `evict_inode` with a WARNING log (never lose an eviction).
//! - If the queue is full AND the caller IS the evict kthread: panic.
//!   Recursive orphan drop on the evict kthread with a full queue means a
//!   runaway; loud failure is correct (D8).
//! - `EVICT_TID` stores the kthread's ThreadId so any code can ask
//!   "am I the evict kthread?" with a single atomic load.

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::{sync::Weak, vec::Vec};
use crossbeam_queue::ArrayQueue;
use spin::Once;

use crate::debug::lock_order::RANK_EVICT_OVERFLOW;
use crate::ranked_lock;
use crate::thread::preempt::PreemptSpinlock;
use crate::thread::scheduler::{current_thread_id, thread_park_while};
use crate::{
    fs::vfs::fs_by_mount_id,
    thread::{
        scheduler::{WakePriority, sched},
        util::queue_spawn_kthread_named,
    },
};

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

const EVICT_QUEUE_CAP: usize = 256;

/// Log one `synchronous fallback` warning per this many occurrences.
const EVICT_FALLBACK_LOG_INTERVAL: u64 = 512;

#[derive(Copy, Clone)]
pub struct EvictRequest {
    pub mount_id: usize,
    pub ino: u64,
}

static EVICT_QUEUE: Once<ArrayQueue<EvictRequest>> = Once::new();
static EVICT_HANDLE: Once<Weak<crate::thread::thread::Thread>> = Once::new();
/// Monotonically increasing count of successful `evict_inode` calls completed
/// by the evict kthread. Used by tests and diagnostics to confirm the kthread
/// is draining the queue. Incremented after each successful call, not on error.
pub static EVICT_DRAIN_COUNT: AtomicU64 = AtomicU64::new(0);

/// ThreadId of the evict kthread, or 0 before it starts.
pub static EVICT_TID: AtomicU64 = AtomicU64::new(0);

/// Evictions the reaper could not fit in the ring, held until the evict
/// kthread drains them.
///
/// The reaper must not block, so it cannot wait for ring space, and it must
/// not perform the eviction itself. Dropping the request instead leaks the
/// inode's blocks until `efs-fsck` runs, which an ordinary create/unlink burst
/// triggered several hundred times per `fsbench` run. Growing the ring only
/// moves the cliff; this has no fixed capacity, so there is no cliff.
///
/// Pushing allocates, which is legal here: the heap is an `IrqSpinlock`, and
/// this path already frees memory as inodes drop.
static EVICT_OVERFLOW: PreemptSpinlock<Vec<EvictRequest>> = PreemptSpinlock::new(Vec::new());

/// Evictions deferred to [`EVICT_OVERFLOW`] because the ring was full on a
/// thread that may not block. Storage is reclaimed once the kthread drains
/// them, so this is queue-pressure telemetry and not a leak count.
pub static EVICT_DROPPED_COUNT: AtomicU64 = AtomicU64::new(0);

/// Evictions the caller had to perform itself because the queue was full.
///
/// A burst of unlinks fills a 256-deep queue in well under a second, so this
/// is counted rather than logged per occurrence: the log line is worth having
/// once, and after that only the rate matters.
pub static EVICT_SYNC_FALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Returns true if the calling thread is the evict-inode kthread.
///
/// Used by debug_assert guards in blocking `Drop` implementations to catch
/// regressions where blocking work fires on the evict kthread (which would
/// cause recursive enqueue or deadlock). Compiled out in release builds.
#[inline]
pub fn current_thread_is_evict_kthread() -> bool {
    let current = current_thread_id().map(|t| t.0).unwrap_or(0);
    current != 0 && current == EVICT_TID.load(Ordering::Acquire)
}

fn evict_queue() -> &'static ArrayQueue<EvictRequest> {
    EVICT_QUEUE.call_once(|| ArrayQueue::new(EVICT_QUEUE_CAP))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Called from `VfsInode::drop`. Posts `(mount_id, ino)` to the evict queue
/// and wakes the evict kthread. Non-blocking on the fast path.
///
/// Falls back to synchronous `evict_inode` if the queue is full and the caller
/// is not the evict kthread. Panics if both conditions hold (D8).
pub fn post_evict(mount_id: usize, ino: u64) {
    match evict_queue().push(EvictRequest { mount_id, ino }) {
        Ok(()) => {
            if let Some(handle) = EVICT_HANDLE.get() {
                sched().wake_thread(handle, WakePriority::Normal);
            }
        }
        Err(_) => {
            // Queue full. The fallback below issues disk I/O, so it is only
            // legal on a thread that may block.
            if current_thread_is_evict_kthread() {
                panic!(
                    "evict queue full on evict kthread -- recursive orphan drop runaway \
                     (mount={}, ino={})",
                    mount_id, ino
                );
            }
            // The reaper reaches this through `VfsInode::drop` while tearing a
            // dead thread down, and must not block: every process exit would
            // queue behind the I/O. Park the request on the unbounded overflow
            // list instead, which the kthread drains ahead of the ring.
            if crate::thread::scheduler::current_thread_is_reaper() {
                EVICT_DROPPED_COUNT.fetch_add(1, Ordering::Relaxed);
                ranked_lock!(RANK_EVICT_OVERFLOW, "evict::overflow", EVICT_OVERFLOW)
                    .push(EvictRequest { mount_id, ino });
                if let Some(handle) = EVICT_HANDLE.get() {
                    sched().wake_thread(handle, WakePriority::Normal);
                }
                return;
            }
            // Synchronous fallback. Counted always, logged sparsely: an unlink
            // burst produces thousands of these, and a per-inode line buries
            // everything else in the log without adding information. A failure
            // is rare and specific, so that one is always logged.
            let Some(fs) = fs_by_mount_id(mount_id) else {
                return;
            };
            let n = EVICT_SYNC_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = fs.evict_inode(ino) {
                crate::log!(
                    "WARNING: evict queue full, synchronous fallback \
                     (mount={}, ino={}) failed: {:?}",
                    mount_id,
                    ino,
                    e
                );
            } else if n % EVICT_FALLBACK_LOG_INTERVAL == 0 {
                crate::log!(
                    "WARNING: evict queue full, synchronous fallback \
                     (mount={}, ino={}); {} so far",
                    mount_id,
                    ino,
                    n + 1
                );
            }
        }
    }
}

/// Read the current drain count. Exposed via `/proc/evict_stats` and useful
/// for tests that want to verify the evict kthread processed at least N
/// requests after a known trigger.
#[inline]
pub fn evict_kthread_drain_count() -> u64 {
    EVICT_DRAIN_COUNT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

/// Initialize the evict subsystem and spawn the evict kthread.
/// Must be called after `init_reaper()` and before any user thread or driver
/// thread that can trigger orphan drops.
pub fn init_evict_kthread() {
    evict_queue(); // Force queue init before kthread starts.

    let tid = queue_spawn_kthread_named("evict-inode", evict_kthread as *const () as u64);
    // Publish EVICT_HANDLE FIRST so any concurrent post_evict can wake us;
    // EVICT_TID second. Readers of the re-entrancy guard race-check against
    // EVICT_TID, so publishing the handle before the TID avoids the window
    // where post_evict sees a valid TID but no wake target.
    EVICT_HANDLE.call_once(|| {
        crate::thread::thread::get_thread_weak(tid)
            .expect("evict kthread vanished before call_once")
    });
    EVICT_TID.store(tid.0, Ordering::Release);
    crate::println!("Evict kthread started (tid={})", tid.0);
}

// ---------------------------------------------------------------------------
// Kthread body
// ---------------------------------------------------------------------------

extern "C" fn evict_kthread() -> ! {
    loop {
        thread_park_while(|| evict_queue().is_empty() && overflow_is_empty());

        // Overflow first: those are the oldest requests, deferred by a reaper
        // that found the ring full. Take the whole list and release the guard
        // before evicting — `evict_inode` performs disk I/O, and the lock is
        // reachable from any thread dropping an inode.
        let overflow = core::mem::take(&mut **ranked_lock!(
            RANK_EVICT_OVERFLOW,
            "evict::overflow drain",
            EVICT_OVERFLOW
        ));
        for req in overflow {
            evict_one(req);
        }

        while let Some(req) = evict_queue().pop() {
            evict_one(req);
        }
    }
}

fn overflow_is_empty() -> bool {
    ranked_lock!(RANK_EVICT_OVERFLOW, "evict::overflow probe", EVICT_OVERFLOW).is_empty()
}

fn evict_one(req: EvictRequest) {
    let Some(fs) = fs_by_mount_id(req.mount_id) else {
        return;
    };
    if let Err(e) = fs.evict_inode(req.ino) {
        crate::log!(
            "evict_kthread: evict_inode(mount={}, ino={}) failed: {:?}",
            req.mount_id,
            req.ino,
            e
        );
    } else {
        EVICT_DRAIN_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}
