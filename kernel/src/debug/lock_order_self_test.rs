/// Lock-order self-test kthreads.
///
/// # Features
///
/// - `lock-order-self-test`: Spawns a kthread that acquires rank 10 then rank
///   20 (correct order) and verifies no panic occurs. Prints
///   "lock-order self-test PASS" and returns normally.
///
/// - `lock-order-self-test-inversion`: Spawns a kthread that acquires rank 20
///   then rank 10 (deliberate inversion). **EXPECTED to panic and halt the
///   machine by design.** Run in isolation with `make run-single`. Do not
///   enable this feature in any shared or production build.
use crate::{
    debug::lock_order::{RANK_VFS, RankedGuard},
    println,
    thread::{irqlock::IrqSpinlock, util::queue_spawn_kthread_named},
};

/// Rank used for the "higher" lock in the self-test (must be > RANK_VFS = 10).
const SELF_TEST_RANK_HIGH: u16 = 20;
const SELF_TEST_SITE_LOW: &str = "self-test-low(10)";
const SELF_TEST_SITE_HIGH: &str = "self-test-high(20)";

// Static locks used as test fixtures (no actual data protected).
static LOCK_LOW: IrqSpinlock<()> = IrqSpinlock::new(());
static LOCK_HIGH: IrqSpinlock<()> = IrqSpinlock::new(());

/// Pass-path kthread body: acquires rank 10, then rank 20. Must not panic.
#[cfg(feature = "lock-order-self-test")]
extern "C" fn self_test_kthread(_arg: u64) {
    // Acquire rank 10 (low) first, then rank 20 (high) — correct order.
    let _guard_low: RankedGuard<_> = LOCK_LOW.lock_ranked(RANK_VFS, SELF_TEST_SITE_LOW);
    let _guard_high: RankedGuard<_> =
        LOCK_HIGH.lock_ranked(SELF_TEST_RANK_HIGH, SELF_TEST_SITE_HIGH);
    // Both guards drop here in reverse declaration order: _guard_high first,
    // then _guard_low. ManuallyDrop ensures lock release before rank pop.
    drop(_guard_high);
    drop(_guard_low);
    println!("lock-order self-test PASS");
    crate::thread::util::kthread_exit(0);
}

/// Spawn the pass-path self-test kthread.
#[cfg(feature = "lock-order-self-test")]
pub fn spawn_self_test() {
    queue_spawn_kthread_named(
        "lock-order-self-test",
        self_test_kthread as *const () as u64,
    );
}

/// Inversion-path kthread body: acquires rank 20, then rank 10 — PANICS.
///
/// **Enabling `lock-order-self-test-inversion` halts the machine by design.**
/// Run in isolation with `make run-single`.
#[cfg(feature = "lock-order-self-test-inversion")]
extern "C" fn inversion_test_kthread(_arg: u64) {
    println!("lock-order-self-test-inversion: about to trigger deliberate panic...");

    // Acquire rank 20 first — this is the "wrong" outer lock.
    let _guard_high: RankedGuard<_> =
        LOCK_HIGH.lock_ranked(SELF_TEST_RANK_HIGH, SELF_TEST_SITE_HIGH);
    // Acquiring rank 10 while holding rank 20 MUST panic with a lock-order
    // violation. The machine halts because the kernel has no catch_unwind.
    let _guard_low: RankedGuard<_> = LOCK_LOW.lock_ranked(RANK_VFS, SELF_TEST_SITE_LOW);

    println!("lock-order-self-test-inversion: ERROR — should have panicked before this line");
    crate::thread::util::kthread_exit(0);
}

/// Spawn the inversion-path self-test kthread (halts the machine by design).
#[cfg(feature = "lock-order-self-test-inversion")]
pub fn spawn_inversion_test() {
    queue_spawn_kthread_named(
        "lock-order-inversion-test",
        inversion_test_kthread as *const () as u64,
    );
}
