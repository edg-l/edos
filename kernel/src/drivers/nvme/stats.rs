//! Driver-wide NVMe counters, rendered by `/proc/nvme_stats`.
//!
//! Every counter is monotonic over a boot and `Relaxed`: they are read for
//! introspection, never to make a decision, so no ordering is needed between
//! them.

use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

use alloc::format;

use crate::{
    drivers::nvme::{api, watchdog},
    interrupts::io::NVME_IRQS_FIRED,
};

/// NVM commands written to an I/O submission queue, split parts counted
/// individually.
pub static COMMANDS_SUBMITTED: AtomicU64 = AtomicU64::new(0);
/// `submit_read`/`submit_write` calls that exceeded the maximum transfer and
/// were chopped into several commands.
pub static SPLIT_REQUESTS: AtomicU64 = AtomicU64::new(0);
/// Commands issued on behalf of a split request. A request split in two
/// adds 2 here and 1 to [`SPLIT_REQUESTS`].
pub static SPLIT_COMMANDS: AtomicU64 = AtomicU64::new(0);
/// Requests that could not use the caller's own pages and went through a
/// bounce buffer.
pub static BOUNCED_REQUESTS: AtomicU64 = AtomicU64::new(0);
/// Flush commands issued. A controller without a volatile write cache
/// leaves this at zero and counts in [`FLUSHES_ELIDED`] instead.
pub static FLUSHES: AtomicU64 = AtomicU64::new(0);
/// Flush requests completed without a command because `Identify
/// Controller` VWC reported no volatile write cache.
pub static FLUSHES_ELIDED: AtomicU64 = AtomicU64::new(0);
/// Commands whose completion carried a non-zero status.
pub static COMMAND_ERRORS: AtomicU64 = AtomicU64::new(0);
/// Pages a PRP entry addressed beyond PRP1, each translated separately by
/// `build_prp`.
pub static PRP_PAGES: AtomicU64 = AtomicU64::new(0);
/// Of those, the ones whose frame was *not* the first page's frame plus the
/// page index: the pages a transfer would have corrupted had `build_prp`
/// derived its addresses by adding 4096 instead of translating. A boot that
/// leaves this at zero has not exercised the translation at all, so it also
/// cannot have caught a regression in it.
pub static PRP_PAGES_DISCONTIGUOUS: AtomicU64 = AtomicU64::new(0);
/// Commands failed without a completion while `CSTS.RDY` was still set, so
/// the controller could still have been writing into the caller's buffer
/// after that buffer was released back to whoever owned it.
///
/// This must stay at zero. It counts a use-after-free whose writer is the
/// device rather than a CPU, which no CPU-side check can see: a recovery
/// path may only abandon a command once `CC.EN` is clear and `CSTS.RDY` has
/// dropped (NVMe 2.0 3.5.1). `make nvme-check`'s watchdog case asserts it.
pub static ABANDONED_WHILE_LIVE: AtomicU64 = AtomicU64::new(0);
/// Commands installed at a command id the queue no longer considers
/// allocated, which means a reset freed it while its submitter still held it.
pub static CID_NOT_HELD_AT_INSTALL: AtomicU64 = AtomicU64::new(0);
/// Commands installed over a slot that already held a live op. Each one is an
/// `NvmeOp` dropped without completing its handle, so its waiter parks
/// forever with nothing outstanding for the watchdog to find.
pub static SLOT_OVERWRITTEN: AtomicU64 = AtomicU64::new(0);
/// Commands `NvmeQueue::reset_state` found still installed, which means a
/// submitter issued them after the caller's own fail-all pass. Non-zero is
/// expected and is not a defect -- nothing excludes a submitter from a reset
/// -- but it measures how often that window is entered, and it used to be the
/// window in which an op was dropped rather than retired.
pub static INSTALLED_DURING_RESET: AtomicU64 = AtomicU64::new(0);

pub fn bump(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

pub fn render() -> String {
    let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
    // Read through the `Once` cells' non-blocking accessors: `/proc` is
    // readable before the probe kthread has published either list, and a
    // `wait()` here would park the reader instead of showing it zeroes.
    let controllers = super::NVME_CONTROLLERS.get().map_or(0, |c| c.len());
    let namespaces = super::NVME_NAMESPACES.get().map_or(0, |n| n.len());
    let (mdts_bytes, vwc) = api::namespaces_if_probed()
        .and_then(|list| list.first())
        .map_or((0, false), |ns| (ns.max_transfer_bytes(), ns.write_cache()));
    format!(
        "controllers={} namespaces={} irqs={} dispatcher_passes={} inflight={} \
         max_inflight={} commands_submitted={} split_requests={} split_commands={} \
         bounced_requests={} flushes={} flushes_elided={} command_errors={} \
         prp_pages={} prp_pages_discontiguous={} abandoned_while_live={} \
         cid_not_held_at_install={} slot_overwritten={} installed_during_reset={} \
         watchdog_firings={} watchdog_completions={} resets={} timeout_ms={} \
         mdts_bytes={} vwc={}\n",
        controllers,
        namespaces,
        get(&NVME_IRQS_FIRED),
        get(&watchdog::DISPATCHER_PASSES),
        get(&watchdog::NVME_INFLIGHT),
        get(&watchdog::NVME_MAX_INFLIGHT),
        get(&COMMANDS_SUBMITTED),
        get(&SPLIT_REQUESTS),
        get(&SPLIT_COMMANDS),
        get(&BOUNCED_REQUESTS),
        get(&FLUSHES),
        get(&FLUSHES_ELIDED),
        get(&COMMAND_ERRORS),
        get(&PRP_PAGES),
        get(&PRP_PAGES_DISCONTIGUOUS),
        get(&ABANDONED_WHILE_LIVE),
        get(&CID_NOT_HELD_AT_INSTALL),
        get(&SLOT_OVERWRITTEN),
        get(&INSTALLED_DURING_RESET),
        get(&watchdog::WATCHDOG_FIRINGS),
        get(&watchdog::WATCHDOG_COMPLETIONS),
        get(&watchdog::WATCHDOG_RESETS),
        get(&watchdog::NVME_TIMEOUT_MS),
        mdts_bytes,
        u8::from(vwc),
    )
}
