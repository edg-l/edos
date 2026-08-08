//! Periodic NCQ watchdog kthread.
//!
//! Why this exists
//! ----------------
//! After Foundation #6 Phase C1, `BlockIoHandle::wait()` is indefinite.
//! Hardware completion is normally driven by the IRQ dispatcher in
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
//! Known residual race
//! --------------------
//! `enter_ncq_mode` does not gate on `AhciPort.restarting`, so a new
//! submitter can install a fresh `AhciNcqOp` into `ncq_waiters[slot]`
//! between the moment the watchdog (or TFES) takes the CAS guard and the
//! moment `restart_port` actually clears `SACT`/`CI`. That fresh op will
//! be silently torpedoed by the port reset and its `BlockIoHandle` is
//! left in `Pending`; the next watchdog tick catches it ~30s later. Worth
//! fixing by having submit paths check `restarting` and back off, but
//! out of scope for the watchdog itself.

use core::{sync::atomic::AtomicU64, time::Duration};

use crate::thread::scheduler::thread_sleep;

/// Timeout after which an in-flight NCQ slot is considered hung.
/// Matches Linux libata's `ATA_TMOUT_NCQ_SEC`.
pub const NCQ_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the watchdog sweeps all ports.
pub const WATCHDOG_TICK: Duration = Duration::from_millis(1000);

/// Total number of watchdog sweep passes executed.
pub static WATCHDOG_FIRINGS: AtomicU64 = AtomicU64::new(0);

/// Total number of port restarts triggered by the watchdog.
pub static WATCHDOG_RESTARTS: AtomicU64 = AtomicU64::new(0);

pub extern "C" fn watchdog_entry() -> ! {
    loop {
        thread_sleep(WATCHDOG_TICK);
        scan_once();
    }
}

fn scan_once() {
    let Some(ports) = super::AHCI_PORTS.get() else {
        return;
    };
    for port in ports.iter() {
        port.watchdog_sweep(NCQ_TIMEOUT);
    }
}
