// Atomics are accessed from both debug and release builds (always-exported).

//! Lock-order rank constants and per-thread enforcement infrastructure.
//!
//! # Invariant
//!
//! Every thread that acquires two or more ranked locks must do so in strictly
//! increasing rank order (lower rank number acquired first). Violations are
//! caught at acquisition time via a debug-only per-thread rank stack.
//!
//! # Rank table (see `doc/invariants/lock-order.md` for full details)
//!
//! | Rank | Lock                                  |
//! |-----:|---------------------------------------|
//! |   10 | `VFS` mount registry                  |
//! |   30 | `inode.lock` (per-inode)              |
//! |   31 | `EfsDriver.inode_rmw`                 |
//! |   32 | `EfsDriver.bitmap_mutex`              |
//! |   35 | `dentry_cache.inner`                  |
//! |   36 | `INODE_CACHE`                         |
//! |   40 | `inode.pages.pages`                   |
//! |   42 | `InodePages.in_flight`                |
//! |   50 | `inode.pages.dirty_keys`              |
//! |   60 | `inode.mappers`                       |
//! |   70 | `UserThread.vmas`                     |
//! |   80 | `UserThread.memory_manager`           |
//! |   90 | `SHARED_MEMORY_REGISTRY`              |
//! |  100 | `DIRTY_INODES`                        |
//! |  110 | `BlockPageCache.shards[N]`            |
//! |  120 | `BlockPageCache.journals`             |
//! |  130 | `Journal.checkpoint_tracker`          |
//! |  140 | `CachedBlockPage.write_lock`          |
//! |  150 | `Journal.state`                       |
//! |  160 | `EfsDriver.mutable`                   |
//! |  170 | `AhciPort.legacy_lock`                |
//! |  172 | `NvmeController.admin`                |
//! |  180 | `AhciPort.slot_waiters[i]`            |
//! |  182 | `NvmeQueue.cmd_slots[i]`              |
//! |  186 | `NvmeQueue.cq`                        |
//! |  190 | `AhciPort.mmio_lock`                  |
//! |  192 | `NvmeQueue.sq`                        |
//! |  200 | `PCI_CONFIG_LOCK`                     |
//! |  204 | `Mailbox.queue`                       |
//! |  206 | `ResponseInner.value`                 |
//! |  210 | `TTY_BUFFER`                          |
//! |  220 | `Pipe` (per-pipe)                     |
//! |  230 | `Pty` (per-pty)                       |
//! |  240 | `NET_STACK`                           |
//! |  250 | `PORT_TABLE`                          |
//! |  260 | `Socket` (per-socket)                 |
//! |  270 | `TcpConnection` (per-connection)      |
//! |  280 | `WINDOW_REGISTRY`                     |
//! |  290 | `WINDOW_EVENTS`                       |
//! |  300 | `LAST_MOUSE_BUTTONS`                  |
//! |  310 | `Broadcaster.subs`                    |
//! |  320 | device poller lists                   |
//! |  330 | `HdaPlaybackState`                    |
//! |  340 | `DevFs.shared`                        |
//! |  900 | kernel-global mapper                  |
//! |  910 | `FRAME_ALLOCATOR`                     |

// ---- Rank constants ---------------------------------------------------------

pub const RANK_VFS: u16 = 10;
pub const RANK_INODE: u16 = 30;
// Serializes an inode's read-modify-write cycle inside EFS. `update_size` and
// `ensure_block*_for_logical` both read an inode, change a different field, and
// write the whole 256-byte struct back; without this the size write drops the
// extents the other just added, orphaning their blocks in the bitmap. Held
// across BPC I/O, so it sits above `inode.lock` (30) and below the alloc mutex
// (32) that block allocation takes underneath it.
pub const RANK_EFS_INODE_RMW: u16 = 31;
pub const RANK_EFS_BITMAP: u16 = 32;
// In-memory mirror of EFS's on-disk orphan chain, which gives an eviction its
// inode's predecessor without walking the chain. Held across the inode and
// superblock writes that unlink an inode from the chain, so it sits below
// `BPC.shard` (110) and `EfsDriver.mutable` (160); never co-held with the bitmap
// mutex, because the chain is updated before storage is freed.
pub const RANK_EFS_ORPHAN: u16 = 33;
pub const RANK_DENTRY: u16 = 35;
// `(mount_id, ino) -> Weak<VfsInode>` map that gives an on-disk inode a single
// `VfsInode`, and with it a single page cache, across dentry invalidations.
// Taken by `resolve_inode_for` after the dentry lookup misses, never while the
// dentry lock is held.
pub const RANK_ICACHE: u16 = 36;
pub const RANK_PAGES: u16 = 40;
pub const RANK_IN_FLIGHT: u16 = 42; // forward ref for Foundation #5
pub const RANK_DIRTY_KEYS: u16 = 50;
pub const RANK_MAPPERS: u16 = 60;
pub const RANK_VMAS: u16 = 70;
pub const RANK_USER_MM: u16 = 80;
// Shared-memory region registry. Inside both address-space locks: `sys_fork`'s
// deep copy resolves each `SharedMemory` VMA's region under the vmas guard, and
// `release_mappings` does the same under the page-table guard while unmapping
// the region's pages.
pub const RANK_SHM_REGISTRY: u16 = 90;
pub const RANK_DIRTY_INODES: u16 = 100;
pub const RANK_BPC_SHARD: u16 = 110;
pub const RANK_BPC_JOURNALS: u16 = 120;
pub const RANK_JOURNAL_TRACKER: u16 = 130;
pub const RANK_PAGE_WRITE_LOCK: u16 = 140;
pub const RANK_JOURNAL_STATE: u16 = 150;
pub const RANK_EFS_MUTABLE: u16 = 160;
pub const RANK_AHCI_LEGACY: u16 = 170;
// NVMe. Interleaved with the AHCI band because an NVMe command is issued
// from the same FS depth an AHCI one is, and the two device stacks are never
// co-held.
//
// `admin` serializes one admin command at a time (init, queue create/delete,
// reset).
pub const RANK_NVME_ADMIN: u16 = 172;
pub const RANK_AHCI_SLOT: u16 = 180;
// The per-command-id op handoff between submit, the IRQ dispatcher, the
// watchdog and cancel -- same shape as `slot_waiters` (180): holders only
// clone or take, never park, never allocate, never take an inner lock.
pub const RANK_NVME_CMD: u16 = 182;
// The whole completion-queue drain pass: phase compare, head advance, phase
// toggle, the CQ head doorbell write. Collects into a bounded `heapless::Vec`
// under the guard and dispatches after releasing it, so it is never co-held
// with `cmd_slots` (182): taking that from inside would be a descending
// acquisition, and it would hold a spin lock across a bounced read's copy
// besides.
pub const RANK_NVME_CQ: u16 = 186;
pub const RANK_AHCI_MMIO: u16 = 190;
// SQ tail cursor, the 64-byte SQE write, the tail doorbell. True leaf.
pub const RANK_NVME_SQ: u16 = 192;
pub const RANK_PCI_CONFIG: u16 = 200;
// Request/response transport between a caller and a driver kthread, used by
// `USB_BLOCK_MAILBOX` and `FS_REQUESTS`. Sits just above the AHCI band because
// a USB block request is issued from the same FS depth an AHCI command is, and
// the two are never co-held. 2-unit spacing: the driver band below and the
// console band above leave no 10-unit gap, and both are leaves.
pub const RANK_MAILBOX_QUEUE: u16 = 204;
pub const RANK_MAILBOX_RESPONSE: u16 = 206;
// IPC and console endpoints. Ranked above the FS ladder because the devfs
// device callbacks that reach `TTY_BUFFER` run under `inode.lock` (30), and
// below the deep leaves because appending to any of these buffers allocates.
// Nothing ranked is acquired while one of them is held.
pub const RANK_TTY_BUFFER: u16 = 210;
// The named-pipe registry, which maps an inode to the `Pipe` its two ends meet
// in. Held across the pipe lock below it while an `open` registers itself, and
// never the other way round: nothing holding a pipe looks a FIFO up. 5-unit
// spacing because the console band below and the pipe itself leave no 10-unit
// gap.
pub const RANK_FIFO_REGISTRY: u16 = 215;
pub const RANK_PIPE: u16 = 220;
pub const RANK_PTY: u16 = 230;
// Networking. The order is fixed by the receive path: `handle_udp`/`handle_tcp`
// run as `&mut self` on the stack, take the port table to find the socket, and
// a socket's `poll_state` locks its connection.
pub const RANK_NET_STACK: u16 = 240;
pub const RANK_PORT_TABLE: u16 = 250;
pub const RANK_SOCKET: u16 = 260;
pub const RANK_TCP_CONN: u16 = 270;
// Window system. The input thread holds the registry across event delivery
// (`handle_keyboard_event`), and nothing on the event-queue side reaches back
// into the registry, so the registry is strictly outside.
//
// The shell table sits *outside* the registry: a syscall decides whether the
// caller may manage other processes' windows before it touches the registry at
// all, and holding both would be two locks of one rank co-held, which the
// tracker rejects.
pub const RANK_WINDOW_SHELL: u16 = 275;
// Claimed key chords, taken by the input thread before it looks up the focused
// window and by the grab syscall after the shell table has settled the
// caller's authority. Outside the registry for the same reason the shell table
// is: routing asks whether a chord is claimed before it asks anything of the
// registry, so the two are never co-held.
pub const RANK_KEY_GRABS: u16 = 276;
pub const RANK_WINDOW_REGISTRY: u16 = 280;
pub const RANK_WINDOW_EVENTS: u16 = 290;
// The clipboard is a leaf of the window band: its two syscalls hold nothing
// else when they take it, and it reaches nothing. Its rank exists to give the
// tracker a total order and to make a guard visible to
// `assert_no_guards_held`, not because anything co-holds it.
pub const RANK_CLIPBOARD: u16 = 295;
pub const RANK_MOUSE_BUTTONS: u16 = 300;
// Device-node state behind devfs, reached from `DevFsDevice` callbacks under
// `inode.lock` (30) and from the driver kthreads that feed them. Above the
// window band because the window input thread is the one context that could
// hold a registry guard while touching input state; the driver kthreads hold
// nothing at all.
pub const RANK_INPUT_SUBS: u16 = 310;
pub const RANK_DEVICE_POLLERS: u16 = 320;
pub const RANK_HDA_STATE: u16 = 330;
// The devfs device registry, above every device lock it dispatches to. Every
// dispatching method releases the guard before calling into the device, and
// ranking it outermost is what makes a future edit that forgets to panic
// instead of quietly reintroducing a spin lock held across a device callback.
pub const RANK_DEVFS_REGISTRY: u16 = 340;
// Orphan-eviction overflow list. Acquired from arbitrary drop contexts,
// including the reaper tearing a dead thread down, so it outranks anything a
// `VfsInode::drop` could still be holding. Never held across a call into a
// filesystem: both sides take it, move the Vec, and release.
pub const RANK_EVICT_OVERFLOW: u16 = 350;
// Syscall trace ring. A true leaf: taken at the syscall entry and exit
// boundaries, where no other guard is live, and never held across a call into
// anything else. Ranked anyway so a thread that dies holding it is caught by
// `assert_no_guards_held` rather than silently corrupting the ring.
pub const RANK_TRACE_RING: u16 = 355;
// Deep utility leaves — acquired from arbitrary contexts (AHCI DMA setup,
// page-table edits, etc.). Must be higher than any caller's rank. Ordered
// relative to each other because `MemoryManager::map_memory` acquires
// frame_alloc while kernel_mapper is held.
pub const RANK_KERNEL_MAPPER: u16 = 900;
pub const RANK_FRAME_ALLOC: u16 = 910;

/// Maximum nesting depth of the per-thread rank stack.
/// 16 is ~2x the deepest observed chain (9 levels).
pub const LOCK_RANK_DEPTH: usize = 16;

// ---- Global observability counters ------------------------------------------
//
// These are incremented in debug builds (inside `enter()`).  They are declared
// without `#[cfg(debug_assertions)]` so that `/proc/lock_order_stats` can read
// them unconditionally; in release builds both counters stay at 0.

use crate::thread::scheduler::{current_thread, current_thread_id};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Number of lock-order violations detected. Should always be 0 in a healthy
/// boot; incremented just before panicking so a swallowed panic is still
/// observable.
pub static LOCK_ORDER_INVERSIONS: AtomicU64 = AtomicU64::new(0);

/// Maximum observed rank-stack depth across all threads and all time.
pub static MAX_RANK_DEPTH: AtomicUsize = AtomicUsize::new(0);

// ---- enter / exit -----------------------------------------------------------

/// Called before acquiring a ranked lock. Verifies `rank > top_rank`.
///
/// No-op in release builds and before the scheduler has a current thread.
#[cfg(debug_assertions)]
pub fn enter(rank: u16, site: &'static str) {
    use crate::thread::scheduler::try_sched;

    // Pre-scheduler init (frame allocator / heap init run before the per-CPU
    // scheduler pointer is installed): no-op.
    if try_sched().is_none() {
        return;
    }
    let Some(thread) = current_thread() else {
        return;
    };

    // Defensive guard: if the Arc's inner pointer lands outside the kernel
    // half (top bit of the 48-bit canonical addr set), the percpu
    // `current_thread` field was torn or uninitialized — e.g. a context
    // switch observed mid-transition. Dereferencing it would NPE-crash the
    // caller (see 2026-04-17 lock_order-null-thread-fault bug). Skip
    // tracking and emit a diagnostic via the lock-bypassing emergency path;
    // a missed tracking entry is harmless, a fault here is catastrophic
    // (nested crash inside a spinlock acquire).
    let ptr = alloc::sync::Arc::as_ptr(&thread) as usize;
    if ptr < 0xffff_8000_0000_0000 {
        crate::serial::emergency_write(b"lock_order: skipping enter due to bogus thread Arc\n");
        return;
    }

    // Single-owner sanity: the thread returned by current_thread() must be us.
    debug_assert_eq!(
        current_thread_id(),
        Some(thread.id),
        "lock_order::enter: current_thread_id mismatch (concurrent access?)"
    );

    // SAFETY: `lock_ranks` is only ever accessed from the owning thread's CPU
    // context (enforced by the `current_thread_id()` check above). No other
    // thread touches this cell.
    let stack = unsafe { &mut *thread.lock_ranks.get() };

    let top = stack.last().copied();
    let top_rank = top.map(|(r, _)| r).unwrap_or(0);

    if rank <= top_rank {
        // Increment the global counter before panicking so the violation is
        // observable even if the panic message is somehow lost.
        LOCK_ORDER_INVERSIONS.fetch_add(1, Ordering::Relaxed);

        // Build the panic message without allocation.
        let mut msg = heapless::String::<256>::new();
        use core::fmt::Write as _;
        let _ = write!(
            msg,
            "lock order violation: tried to acquire '{}' (rank {}) \
             while holding '{}' (rank {}); full stack: [",
            site,
            rank,
            top.map(|(_, s)| s).unwrap_or("<none>"),
            top_rank,
        );
        for (i, (r, s)) in stack.iter().enumerate() {
            if i > 0 {
                let _ = write!(msg, ", ");
            }
            let _ = write!(msg, "{s}({r})");
        }
        let _ = write!(msg, "]");
        panic!("{}", msg.as_str());
    }

    stack
        .push((rank, site))
        .expect("lock_order stack overflow (depth > LOCK_RANK_DEPTH)");

    // Update the global max-depth counter.
    let depth = stack.len();
    let _ = MAX_RANK_DEPTH.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev| {
        if depth > prev { Some(depth) } else { None }
    });
}

/// Called before acquiring a same-class different-instance ranked lock.
/// Asserts `rank >= top_rank` instead of strictly greater, allowing same-rank
/// acquisition (e.g. two inodes in `vfs::rename`).
///
/// **This is NOT reentrance protection.** The caller is responsible for
/// key-ordering the two instances. Wrong-key-order same-class deadlocks are
/// not detectable by the rank system.
#[cfg(debug_assertions)]
pub fn enter_same(rank: u16, site: &'static str) {
    use crate::thread::scheduler::try_sched;

    if try_sched().is_none() {
        return;
    }
    let Some(thread) = current_thread() else {
        return;
    };

    // See `enter()` for rationale: bogus Arc pointer means skip tracking.
    let ptr = alloc::sync::Arc::as_ptr(&thread) as usize;
    if ptr < 0xffff_8000_0000_0000 {
        crate::serial::emergency_write(
            b"lock_order: skipping enter_same due to bogus thread Arc\n",
        );
        return;
    }

    debug_assert_eq!(
        current_thread_id(),
        Some(thread.id),
        "lock_order::enter_same: current_thread_id mismatch"
    );

    let stack = unsafe { &mut *thread.lock_ranks.get() };

    let top = stack.last().copied();
    let top_rank = top.map(|(r, _)| r).unwrap_or(0);

    if rank < top_rank {
        let mut msg = heapless::String::<256>::new();
        use core::fmt::Write as _;
        let _ = write!(
            msg,
            "lock order violation (same-rank path): tried to acquire '{}' (rank {}) \
             while holding '{}' (rank {})",
            site,
            rank,
            top.map(|(_, s)| s).unwrap_or("<none>"),
            top_rank,
        );
        panic!("{}", msg.as_str());
    }

    stack
        .push((rank, site))
        .expect("lock_order stack overflow (depth > LOCK_RANK_DEPTH)");
}

/// Called after dropping a ranked lock guard. Pops the rank stack and asserts
/// the popped entry matches the expected `(rank, site)`.
#[cfg(debug_assertions)]
pub fn exit(rank: u16, site: &'static str) {
    use crate::thread::scheduler::try_sched;

    if try_sched().is_none() {
        return;
    }
    let Some(thread) = current_thread() else {
        return;
    };

    // See `enter()` for rationale: bogus Arc pointer means skip tracking.
    // This mirrors `enter`'s skip so the paired push/pop stay balanced when
    // a Foundation-#4 tracking window straddles a torn-read; if `enter`
    // skipped, `exit` must also skip or the stack underflows.
    let ptr = alloc::sync::Arc::as_ptr(&thread) as usize;
    if ptr < 0xffff_8000_0000_0000 {
        crate::serial::emergency_write(b"lock_order: skipping exit due to bogus thread Arc\n");
        return;
    }

    let stack = unsafe { &mut *thread.lock_ranks.get() };

    match stack.pop() {
        Some((r, s)) => {
            debug_assert_eq!(
                (r, s),
                (rank, site),
                "lock_order::exit: popped ({s}, {r}) but expected ({site}, {rank}); \
                 guards dropped out of order?"
            );
        }
        None => {
            panic!(
                "lock_order::exit: rank stack underflow when exiting '{}' (rank {})",
                site, rank
            );
        }
    }
}

/// Assert the calling thread holds no ranked lock, at a point where it is about
/// to stop running for good.
///
/// There is no unwinding here, so a thread that dies holding a guard leaks it
/// permanently: the lock is never released and every later acquirer blocks
/// forever. The sibling of the drop contract — that one says a `Drop` must not
/// block, this one says a guard must not be live where a thread can die.
///
/// Covers ranked locks only, which is what the per-thread stack records. A
/// guard on an unranked lock (`Thread.owned_ops`, `WaitQueue.inner`, the
/// scheduler's own locks) is invisible here; ranking a lock is what widens it.
#[cfg(debug_assertions)]
pub fn assert_no_guards_held(context: &str) {
    use crate::thread::scheduler::try_sched;

    if try_sched().is_none() {
        return;
    }
    let Some(thread) = current_thread() else {
        return;
    };

    // See `enter()` for rationale: bogus Arc pointer means skip tracking, so
    // the stack it would report is not trustworthy either.
    let ptr = alloc::sync::Arc::as_ptr(&thread) as usize;
    if ptr < 0xffff_8000_0000_0000 {
        crate::serial::emergency_write(
            b"lock_order: skipping exit check due to bogus thread Arc\n",
        );
        return;
    }

    let stack = unsafe { &mut *thread.lock_ranks.get() };
    if stack.is_empty() {
        return;
    }

    let mut msg = heapless::String::<256>::new();
    use core::fmt::Write as _;
    let _ = write!(
        msg,
        "thread {} died at {} holding {} ranked guard(s), innermost '{}' (rank {}); \
         a guard live where a thread can die leaks the lock permanently",
        thread.id.0,
        context,
        stack.len(),
        stack.last().map(|(_, s)| *s).unwrap_or("<none>"),
        stack.last().map(|(r, _)| *r).unwrap_or(0),
    );
    panic!("{}", msg.as_str());
}

// ---- no-op release stubs ----------------------------------------------------

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn assert_no_guards_held(_context: &str) {}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn enter(_rank: u16, _site: &'static str) {}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn enter_same(_rank: u16, _site: &'static str) {}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn exit(_rank: u16, _site: &'static str) {}

// ---- RankedGuard ------------------------------------------------------------

use core::mem::ManuallyDrop;

/// RAII wrapper for a ranked lock guard.
///
/// On drop, the inner guard is released FIRST (physically releasing the lock),
/// then `exit()` is called to pop the rank from the per-thread stack.
/// This ordering is guaranteed by the `ManuallyDrop<G>` implementation and
/// cannot be broken by field-reordering during refactoring.
pub struct RankedGuard<G> {
    inner: ManuallyDrop<G>,
    #[cfg(debug_assertions)]
    rank: u16,
    #[cfg(debug_assertions)]
    site: &'static str,
}

impl<G> RankedGuard<G> {
    #[inline]
    pub fn new(inner: G, _rank: u16, _site: &'static str) -> Self {
        Self {
            inner: ManuallyDrop::new(inner),
            #[cfg(debug_assertions)]
            rank: _rank,
            #[cfg(debug_assertions)]
            site: _site,
        }
    }
}

impl<G> core::ops::Deref for RankedGuard<G> {
    type Target = G;
    #[inline]
    fn deref(&self) -> &G {
        &self.inner
    }
}

impl<G> core::ops::DerefMut for RankedGuard<G> {
    #[inline]
    fn deref_mut(&mut self) -> &mut G {
        &mut self.inner
    }
}

impl<G> Drop for RankedGuard<G> {
    fn drop(&mut self) {
        // SAFETY: `inner` is not accessed after this point; `self` is being
        // dropped. Dropping inner first releases the lock physically before we
        // pop the rank stack, preserving the invariant that the stack reflects
        // locks that are actually held.
        unsafe { ManuallyDrop::drop(&mut self.inner) };

        #[cfg(debug_assertions)]
        exit(self.rank, self.site);
    }
}

// ---- Macros -----------------------------------------------------------------

/// Acquire a ranked lock via `.lock()`, returning a `RankedGuard`.
///
/// Usage: `ranked_lock!(RANK_PAGES, "InodePages.pages", inode.pages.pages)`
///
/// The macro calls `enter(rank, site)` before locking, then wraps the guard.
/// On drop, the guard releases the lock and calls `exit(rank, site)`.
#[macro_export]
macro_rules! ranked_lock {
    ($rank:expr, $site:expr, $lock_expr:expr) => {{
        $crate::debug::lock_order::enter($rank, $site);
        let inner = $lock_expr.lock();
        $crate::debug::lock_order::RankedGuard::new(inner, $rank, $site)
    }};
}

/// Acquire a same-class different-instance ranked lock via `.lock()`.
///
/// Use only when acquiring two locks of the same rank class (e.g. two inodes
/// in `vfs::rename`). The caller MUST key-order the acquisitions externally
/// (e.g. by inode number). This macro permits `rank >= top` rather than
/// the usual `rank > top`.
///
/// **NOT reentrance protection.** Wrong-key-order deadlocks are the caller's
/// responsibility and cannot be detected by the rank system.
#[macro_export]
macro_rules! ranked_lock_same {
    ($rank:expr, $site:expr, $lock_expr:expr) => {{
        $crate::debug::lock_order::enter_same($rank, $site);
        let inner = $lock_expr.lock();
        $crate::debug::lock_order::RankedGuard::new(inner, $rank, $site)
    }};
}

/// Acquire a ranked `RwLock` for reading via `.read()`, returning a `RankedGuard`.
///
/// Usage: `ranked_read!(RANK_VFS, "VFS", VFS)`
#[macro_export]
macro_rules! ranked_read {
    ($rank:expr, $site:expr, $lock_expr:expr) => {{
        $crate::debug::lock_order::enter($rank, $site);
        let inner = $lock_expr.read();
        $crate::debug::lock_order::RankedGuard::new(inner, $rank, $site)
    }};
}

/// Acquire a ranked `RwLock` for writing via `.write()`, returning a `RankedGuard`.
///
/// Usage: `ranked_write!(RANK_VFS, "VFS", VFS)`
#[macro_export]
macro_rules! ranked_write {
    ($rank:expr, $site:expr, $lock_expr:expr) => {{
        $crate::debug::lock_order::enter($rank, $site);
        let inner = $lock_expr.write();
        $crate::debug::lock_order::RankedGuard::new(inner, $rank, $site)
    }};
}
