//! Periodic NCQ watchdog kthread.
//!
//! Why this exists
//! ----------------
//! `BlockIoHandle::wait()` is indefinite. Hardware completion is normally driven by the IRQ dispatcher in
//! `ahci_driver_main`, or, on a drive error, by the TFES path
//! (`fail_all_ncq_slots` + `restart_port`). If a SATA drive hangs without
//! either an IRQ or a TFES (bad sector retry exhaustion, firmware bug,
//! link glitch without a CRC error), the waiter blocks forever.
//!
//! This watchdog is the last-resort safety net: it sweeps every port at a
//! fixed cadence, finds in-flight NCQ slots whose `issue_time` is older
//! than `NCQ_TIMEOUT`, and recovers them via the same code path TFES uses.
//!
//! Recovery rationale
//! -------------------
//! AHCI has no per-slot abort. Once a port is in an error state, NCQ slot
//! ordering is undefined; the only well-defined recovery is a port reset
//! (COMRESET) which fails every in-flight op as collateral. This matches
//! Linux libata's behavior (`ata_eh_reset`).
//!
//! Timeout choice
//! ---------------
//! 30 seconds matches Linux libata's `ATA_TMOUT_NCQ_SEC`. Real spinning
//! disks can legitimately take 3-5 seconds for a marginal-sector remap;
//! 5 seconds would produce false positives. 30 seconds is the standard.
//!
//! Racing a restart against a submit
//! ---------------------------------
//! A restart round and a submit in progress cover each other by ordering.
//! The restarter bumps `reset_generation` *before* `fail_all_ncq_slots`,
//! and the submitter re-reads it *after* storing `issued`. Either the
//! submitter observes the bump and completes its own slot, or its
//! `issued` store precedes the fail-all pass, which then fails the op.
//! Neither leaves an op pending for a later sweep to find.
//!
//! `enter_ncq_mode` also refuses admission while `restarting` is set, so
//! a submit that has not started yet waits for the reset instead of being
//! issued into a port that is about to be torn down. That is a
//! throughput measure, not the correctness one: a submitter already past
//! the gate is what the ordering above exists for.

use core::{sync::atomic::AtomicU64, time::Duration};

use crate::thread::scheduler::thread_sleep;

/// Timeout after which an in-flight NCQ slot is considered hung.
/// Matches Linux libata's `ATA_TMOUT_NCQ_SEC`.
pub const NCQ_TIMEOUT: Duration = Duration::from_secs(30);

/// Effective timeout in milliseconds, so a boot can shorten it.
///
/// `ahci_ncq_timeout_ms=<n>` on the kernel command line drives the watchdog
/// into restarting ports underneath live I/O, which is the only way to
/// exercise the submit-versus-restart ordering at any useful rate: the real
/// 30 s timeout needs a drive that has actually hung. Every restart it
/// causes fails legitimate in-flight commands, so it is a test setting.
pub static NCQ_TIMEOUT_MS: AtomicU64 = AtomicU64::new(NCQ_TIMEOUT.as_millis() as u64);

/// Ops a sweep found still pending from an *earlier* reset generation.
///
/// This is the fingerprint of a submit stranded by a restart: the op was
/// torpedoed by a port reset that had already run its fail-all pass, so
/// nothing completed it and it waited here for a later sweep. Any non-zero
/// value means the ordering in the module docs was violated.
pub static NCQ_STRANDED: AtomicU64 = AtomicU64::new(0);

/// Apply `ahci_ncq_timeout_ms=<n>` from the kernel command line.
///
/// Zero means every op a sweep finds in flight is treated as hung, which is
/// the only setting that reliably lands a port reset inside a submit: a real
/// NCQ command against a backing file completes in well under a millisecond,
/// so any positive timeout large enough to be sane is never reached.
pub fn set_ncq_timeout_ms(ms: u64) {
    NCQ_TIMEOUT_MS.store(ms, core::sync::atomic::Ordering::Relaxed);
}

fn ncq_timeout() -> Duration {
    Duration::from_millis(NCQ_TIMEOUT_MS.load(core::sync::atomic::Ordering::Relaxed))
}

/// How often the watchdog sweeps all ports.
pub const WATCHDOG_TICK: Duration = Duration::from_millis(1000);

/// Total number of watchdog sweep passes executed.
pub static WATCHDOG_FIRINGS: AtomicU64 = AtomicU64::new(0);

/// Total number of port restarts triggered by the watchdog.
pub static WATCHDOG_RESTARTS: AtomicU64 = AtomicU64::new(0);

/// NCQ commands currently issued and not yet completed, across all ports.
pub static NCQ_INFLIGHT: AtomicU64 = AtomicU64::new(0);

/// High-water mark of [`NCQ_INFLIGHT`].
///
/// This is how you tell a path that submits one command at a time from one
/// that fills the queue: a sequential read of many pages should drive this to
/// the negotiated depth, and a peak of 1 means every command waited for the
/// one before it.
pub static NCQ_MAX_INFLIGHT: AtomicU64 = AtomicU64::new(0);

/// Record one NCQ command becoming outstanding.
pub fn ncq_inflight_inc() {
    let now = NCQ_INFLIGHT.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    NCQ_MAX_INFLIGHT.fetch_max(now, core::sync::atomic::Ordering::Relaxed);
}

/// Record one NCQ command completing.
pub fn ncq_inflight_dec() {
    NCQ_INFLIGHT.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
}

pub extern "C" fn watchdog_entry() -> ! {
    loop {
        // Sweep at the tick, or faster when the timeout has been shortened
        // for a test, so a short timeout is actually reachable.
        thread_sleep(WATCHDOG_TICK.min(ncq_timeout().max(Duration::from_millis(1))));
        scan_once();
    }
}

fn scan_once() {
    let Some(ports) = super::AHCI_PORTS.get() else {
        return;
    };
    for port in ports.iter() {
        port.watchdog_sweep(ncq_timeout());
    }
}
