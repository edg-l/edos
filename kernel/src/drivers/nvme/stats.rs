//! Driver-wide NVMe counters, rendered by `/proc/nvme_stats`.
//!
//! Every counter is monotonic over a boot and `Relaxed`: they are read for
//! introspection, never to make a decision, so no ordering is needed between
//! them.

use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

use alloc::format;

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

pub fn bump(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

pub fn render() -> String {
    let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
    format!(
        "commands_submitted={} split_requests={} split_commands={} bounced_requests={} \
         flushes={} flushes_elided={} command_errors={}\n",
        get(&COMMANDS_SUBMITTED),
        get(&SPLIT_REQUESTS),
        get(&SPLIT_COMMANDS),
        get(&BOUNCED_REQUESTS),
        get(&FLUSHES),
        get(&FLUSHES_ELIDED),
        get(&COMMAND_ERRORS),
    )
}
