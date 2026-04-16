//! Smoke test for the `CancellableOp` registry (Foundation #2).
//!
//! Enabled by the `cancel-smoke` Cargo feature.  Not compiled into production
//! builds.  Exercises the minimal path: a kthread exits immediately (before
//! completing the op), and the cancel path bumps an atomic.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::{
    println,
    thread::{
        cancel::{ArcCancellableOp, CancellableOp},
        scheduler::sched,
        util::queue_spawn_kthread_named_arg,
    },
};

// ---------------------------------------------------------------------------
// TestOp
// ---------------------------------------------------------------------------

struct TestOp {
    cancel_count: Arc<AtomicU32>,
}

impl CancellableOp for TestOp {
    fn cancel(&self) {
        self.cancel_count.fetch_add(1, Ordering::Release);
    }

    fn id(&self) -> (&'static str, u64) {
        ("cancel-smoke", 0)
    }
}

// ---------------------------------------------------------------------------
// Kthread body
// ---------------------------------------------------------------------------

/// The kthread: registers a `TestOp` in `owned_ops`, then exits immediately.
/// `Thread::free` should drain `owned_ops` and call `cancel()` on the `TestOp`,
/// bumping the counter.
extern "C" fn smoke_kthread(arg: u64) -> ! {
    let cancel_count_ptr = arg as *const AtomicU32;
    // SAFETY: caller owns the Arc and the kthread is joined via sleep.
    let cancel_count = unsafe { Arc::from_raw(cancel_count_ptr) };

    let op: ArcCancellableOp = Arc::new(TestOp {
        cancel_count: Arc::clone(&cancel_count),
    });

    // Register BEFORE "parking" (here we just exit immediately, which
    // exercises the death path rather than the normal-completion path).
    let current = sched()
        .current_thread()
        .expect("smoke_kthread: no current thread");
    let _ = current.owned_ops_push(op);

    // Intentionally do NOT call owned_ops_remove — simulate death-before-completion.
    sched().thread_exit(0);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the cancel smoke test.  Call once from main (before drivers, after scheduler init).
pub fn run_cancel_smoke() {
    println!("cancel-smoke: starting");

    let cancel_count = Arc::new(AtomicU32::new(0));

    // Pass a raw pointer (into the Arc) as the kthread arg.
    let raw = Arc::into_raw(Arc::clone(&cancel_count)) as *mut u8;
    queue_spawn_kthread_named_arg("cancel-smoke", smoke_kthread as *const () as u64, raw);

    // Wait long enough for the kthread to exit and be reaped.
    for _ in 0..1_000 {
        sched().thread_yield();
    }

    let count = cancel_count.load(Ordering::Acquire);
    if count == 1 {
        println!("cancel-smoke: PASS (cancel() called {} time(s))", count);
    } else {
        panic!(
            "cancel-smoke: FAIL (cancel() called {} time(s), expected 1)",
            count
        );
    }
}
