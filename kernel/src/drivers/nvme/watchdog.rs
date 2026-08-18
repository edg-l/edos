//! Periodic NVMe watchdog kthread.
//!
//! Why this exists
//! ----------------
//! `BlockIoHandle::wait()` is indefinite, and this driver's only completion
//! source is the MSI-X vector the dispatcher parks on. A lost or
//! mis-targeted interrupt, or a controller that stops posting completions
//! altogether, leaves every waiter parked forever with nothing to notice.
//!
//! Recovery rationale
//! -------------------
//! NVMe 2.0 offers no reliable per-command abort (Abort is advisory and may
//! be silently ignored), so the only well-defined recovery is a controller
//! reset, which fails every outstanding command as collateral -- the same
//! trade AHCI makes with COMRESET.
//!
//! A sweep drains the I/O completion queue *before* it judges anything
//! stale. If commands were sitting completed in the CQ, the device did its
//! job and the interrupt was lost; that is counted in
//! [`WATCHDOG_COMPLETIONS`] and costs nothing but a tick of latency, where
//! a reset would have failed live I/O for no reason.
//!
//! The admin queue is deliberately not drained here. `admin_command_polled`
//! polls its own completion, and a drain from this thread would consume the
//! entry that poll is waiting for, turning a healthy admin command into a
//! timeout.
//!
//! Timeout choice
//! ---------------
//! 30 seconds matches Linux's `NVME_IO_TIMEOUT` default and AHCI's
//! `NCQ_TIMEOUT` here, so a machine with both drivers reports a hung disk on
//! the same schedule whichever bus it is on.

use core::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::{drivers::nvme::api, thread::scheduler::thread_sleep};

/// Timeout after which an outstanding command is considered hung.
pub const NVME_TIMEOUT: Duration = Duration::from_secs(30);

/// Effective timeout in milliseconds, so a boot can shorten it.
///
/// `nvme_timeout_ms=<n>` on the kernel command line drives the watchdog into
/// resetting a controller underneath live I/O, which is the only way to
/// exercise the reset path at any useful rate: a real command against a
/// backing file completes in well under a millisecond, so any sane positive
/// timeout is never reached. Every reset it causes fails legitimate
/// in-flight commands, so it is a test setting.
pub static NVME_TIMEOUT_MS: AtomicU64 = AtomicU64::new(NVME_TIMEOUT.as_millis() as u64);

/// Apply `nvme_timeout_ms=<n>` from the kernel command line. Zero treats
/// every outstanding command a sweep finds as hung.
pub fn set_nvme_timeout_ms(ms: u64) {
    NVME_TIMEOUT_MS.store(ms, Ordering::Relaxed);
}

pub fn nvme_timeout() -> Duration {
    Duration::from_millis(NVME_TIMEOUT_MS.load(Ordering::Relaxed))
}

/// How often the watchdog sweeps every controller.
pub const WATCHDOG_TICK: Duration = Duration::from_millis(1000);

/// Commands a sweep declared hung. Unlike AHCI's counter of the same name
/// this counts *ops*, not sweep passes: a pass that finds nothing is not
/// news, and the gate for this path is that a firing happened at all.
pub static WATCHDOG_FIRINGS: AtomicU64 = AtomicU64::new(0);

/// Controller resets the watchdog completed.
pub static WATCHDOG_RESETS: AtomicU64 = AtomicU64::new(0);

/// Completions a sweep found sitting in a completion queue that the
/// dispatcher had not been woken for. Non-zero means interrupts are being
/// lost and the watchdog is covering for them.
pub static WATCHDOG_COMPLETIONS: AtomicU64 = AtomicU64::new(0);

/// Commands issued to a controller and not yet retired.
pub static NVME_INFLIGHT: AtomicU64 = AtomicU64::new(0);

/// High-water mark of [`NVME_INFLIGHT`]. A peak of 1 means every command
/// waited for the one before it rather than filling the queue.
pub static NVME_MAX_INFLIGHT: AtomicU64 = AtomicU64::new(0);

/// Dispatcher wakes that drained at least one queue.
pub static DISPATCHER_PASSES: AtomicU64 = AtomicU64::new(0);

pub fn inflight_inc() {
    let now = NVME_INFLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
    NVME_MAX_INFLIGHT.fetch_max(now, Ordering::Relaxed);
}

pub fn inflight_dec() {
    NVME_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
}

pub extern "C" fn watchdog_entry() -> ! {
    loop {
        // Sweep at the tick, or faster when the timeout has been shortened
        // for a test, so a short timeout is actually reachable.
        thread_sleep(WATCHDOG_TICK.min(nvme_timeout().max(Duration::from_millis(1))));
        for controller in api::controllers() {
            controller.watchdog_sweep(nvme_timeout());
        }
    }
}
