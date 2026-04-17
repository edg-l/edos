// Phase 2 instruments call sites; allow unused items until then.
#![allow(dead_code)]

/// Lock-order rank constants and per-thread enforcement infrastructure.
///
/// # Invariant
///
/// Every thread that acquires two or more ranked locks must do so in strictly
/// increasing rank order (lower rank number acquired first). Violations are
/// caught at acquisition time via a debug-only per-thread rank stack.
///
/// # Rank table (see `doc/invariants/lock-order.md` for full details)
///
/// | Rank | Lock                                  |
/// |-----:|---------------------------------------|
/// |   10 | `VFS` mount registry                  |
/// |   30 | `inode.lock` (per-inode)              |
/// |   35 | `dentry_cache.inner`                  |
/// |   40 | `inode.pages.pages`                   |
/// |   42 | `InodePages.in_flight` (Foundation #5)|
/// |   50 | `inode.pages.dirty_keys`              |
/// |   60 | `inode.mappers`                       |
/// |   70 | `UserThread.vmas`                     |
/// |   80 | `UserThread.memory_manager`           |
/// |   85 | kernel-global mapper                  |
/// |   90 | `FRAME_ALLOCATOR`                     |
/// |  100 | `DIRTY_INODES`                        |
/// |  110 | `BlockPageCache.shards[N]`            |
/// |  120 | `BlockPageCache.journals`             |
/// |  130 | `Journal.checkpoint_tracker`          |
/// |  140 | `CachedBlockPage.write_lock`          |
/// |  150 | `Journal.state`                       |
/// |  160 | `EfsDriver.mutable`                   |
/// |  170 | `AhciPort.legacy_lock`                |
/// |  180 | `AhciPort.slot_waiters[i]`            |
/// |  190 | `AhciPort.mmio_lock`                  |
/// |  200 | `PCI_CONFIG_LOCK`                     |

// ---- Rank constants ---------------------------------------------------------

pub const RANK_VFS: u16 = 10;
pub const RANK_INODE: u16 = 30;
pub const RANK_DENTRY: u16 = 35;
pub const RANK_PAGES: u16 = 40;
pub const RANK_IN_FLIGHT: u16 = 42; // forward ref for Foundation #5
pub const RANK_DIRTY_KEYS: u16 = 50;
pub const RANK_MAPPERS: u16 = 60;
pub const RANK_VMAS: u16 = 70;
pub const RANK_USER_MM: u16 = 80;
pub const RANK_KERNEL_MAPPER: u16 = 85;
pub const RANK_FRAME_ALLOC: u16 = 90;
pub const RANK_DIRTY_INODES: u16 = 100;
pub const RANK_BPC_SHARD: u16 = 110;
pub const RANK_BPC_JOURNALS: u16 = 120;
pub const RANK_JOURNAL_TRACKER: u16 = 130;
pub const RANK_PAGE_WRITE_LOCK: u16 = 140;
pub const RANK_JOURNAL_STATE: u16 = 150;
pub const RANK_EFS_MUTABLE: u16 = 160;
pub const RANK_AHCI_LEGACY: u16 = 170;
pub const RANK_AHCI_SLOT: u16 = 180;
pub const RANK_AHCI_MMIO: u16 = 190;
pub const RANK_PCI_CONFIG: u16 = 200;

/// Maximum nesting depth of the per-thread rank stack.
/// 16 is ~2x the deepest observed chain (9 levels).
pub const LOCK_RANK_DEPTH: usize = 16;

// ---- enter / exit -----------------------------------------------------------

/// Called before acquiring a ranked lock. Verifies `rank > top_rank`.
///
/// No-op in release builds and before the scheduler has a current thread.
#[cfg(debug_assertions)]
pub fn enter(rank: u16, site: &'static str) {
    use crate::thread::scheduler::sched;

    let Some(thread) = sched().current_thread() else {
        // Pre-scheduler init: no-op.
        return;
    };

    // Single-owner sanity: the thread returned by current_thread() must be us.
    debug_assert_eq!(
        sched().current_thread_id(),
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
    use crate::thread::scheduler::sched;

    let Some(thread) = sched().current_thread() else {
        return;
    };

    debug_assert_eq!(
        sched().current_thread_id(),
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
    use crate::thread::scheduler::sched;

    let Some(thread) = sched().current_thread() else {
        return;
    };

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

// ---- no-op release stubs ----------------------------------------------------

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
