//! Cancellable async operation trait and per-thread owned-op registry.
//!
//! `CancellableOp` is implemented by any async in-flight operation that a
//! thread "owns" while parked waiting for completion.  On thread death,
//! `Thread::free` calls `Thread::owned_ops_cancel_all`, which drains the
//! registry and calls `cancel()` on each entry.
//!
//! # Semantics
//!
//! - **`owned_ops_push`**: called by the submitter BEFORE parking.  Outside
//!   scheduler/IRQ paths — heap allocation is allowed.
//! - **`owned_ops_remove`**: called by the driver (or the submitter after wake)
//!   to deregister a completed op.
//! - **`owned_ops_cancel_all`**: called from `Thread::free` on the reaper
//!   kthread.  Drains the registry, releases the lock, then calls `cancel()`.
//!   `cancel()` implementations must be non-blocking.
//!
//! # Overflow
//!
//! If `owned_ops_push` returns `Err(op)` (registry full), the caller falls
//! back to the pre-Foundation-#2 behaviour (no cancellation hookup for that
//! op).  With `OWNED_OPS_CAP = 32` this is exceedingly rare.

use alloc::sync::Arc;

/// Maximum number of in-flight cancellable ops a single thread can own.
/// `ncq_read_batch` can hold up to 31 simultaneous slots; 32 covers that
/// plus one slot for any concurrent non-NCQ op.
pub const OWNED_OPS_CAP: usize = 32;

/// Trait for async operations that must be explicitly cancelled when the
/// owning thread dies before the operation completes.
///
/// Implementations are held under an `IrqSpinlock` and must be non-blocking.
/// They may be called concurrently with normal completion and must therefore
/// be idempotent (typically via an `AtomicU8` state CAS).
pub trait CancellableOp: Send + Sync {
    /// Called from `Thread::free` when the owner died before completion.
    ///
    /// Must be non-blocking.  Implementations take ownership of hardware
    /// teardown (release AHCI slot, detach mailbox response, …).  May race
    /// with normal completion; must be idempotent.
    fn cancel(&self);

    /// Stable identifier for debug / dedup.  Returns `(driver-tag, resource-id)`.
    #[allow(dead_code)]
    fn id(&self) -> (&'static str, u64) {
        ("anon", 0)
    }
}

/// Type alias for a heap-allocated, reference-counted cancellable op.
pub type ArcCancellableOp = Arc<dyn CancellableOp>;
