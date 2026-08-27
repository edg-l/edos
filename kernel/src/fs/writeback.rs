//! Background writeback kthread for the block page cache and MAP_SHARED inodes.
//!
//! Spawned once during boot after AHCI drivers are initialized. Loops forever:
//!   - Waits up to 5 seconds on `writeback_wq` for a kick or periodic tick.
//!   - A timer expiry runs an unforced pass, which respects `dirty_expire` and
//!     leaves recently-dirtied pages alone.
//!   - A kick (`sync`, `fsync`, `flush_device`) runs a forced pass that writes
//!     every dirty page, then publishes the request number and wakes waiters.
//!
//! The two kinds of pass are kept strictly apart. `flush_requested` /
//! `flush_completed` count only kicks, so an unforced periodic pass can never
//! satisfy a caller waiting for durability: that caller would go on to drop
//! pages the pass had skipped.

use core::{sync::atomic::Ordering, time::Duration};

use crate::{
    fs::{block_page_cache::BlockPageCache, vfs::flush_dirty_inodes},
    log, log_debug,
};

/// Run one pass and account for it. Returns bytes written.
///
/// Three steps, because the two caches feed each other in one direction only:
/// flushing a file page writes *through* the block page cache, so it creates
/// block-cache dirt, while a block flush creates nothing.
///
///   1. Drain the block cache. This also frees shard capacity, without which
///      step 2 gets nothing but detached pages and makes no progress.
///   2. Flush file pages from the inode page cache and MAP_SHARED mappings.
///   3. Drain the block cache again, for what step 2 just dirtied.
///
/// Skipping step 3 is what makes a `sync` return with the caller's last write
/// still in memory: the size lands (it is written synchronously) but the data
/// and the metadata the flush allocated do not.
fn run_pass(cache: &BlockPageCache, force: bool) -> bool {
    let (mut bytes, ok_a) = flush_blocks(cache, force);
    flush_dirty_inodes();
    let (more, ok_b) = flush_blocks(cache, force);
    bytes += more;

    cache.stats.writeback_runs.fetch_add(1, Ordering::Relaxed);
    cache
        .stats
        .writeback_bytes
        .fetch_add(bytes, Ordering::Relaxed);
    if bytes > 0 {
        log_debug!("writeback: flushed {} bytes", bytes);
    }
    ok_a && ok_b
}

/// Returns the bytes written and whether every page the pass attempted was
/// written. A pass that reports `false` left dirty pages behind.
fn flush_blocks(cache: &BlockPageCache, force: bool) -> (u64, bool) {
    match cache.flush_dirty_once(force) {
        Ok(b) => (b, true),
        Err(e) => {
            log!("writeback: flush error {:?}", e);
            (0, false)
        }
    }
}

pub fn writeback_thread() -> ! {
    // A poller that finds nothing is not the machine making progress.
    #[cfg(feature = "stall-dump")]
    crate::debug::stall::mark_heartbeat();
    loop {
        let cache = BlockPageCache::global();

        let req = cache.flush_requested.load(Ordering::Acquire);
        let done = cache.flush_completed.load(Ordering::Acquire);

        if req == done {
            // Nothing pending; sleep up to 5 s waiting for a kick.
            cache.writeback_wq.wait_until_timeout(
                || {
                    cache.flush_requested.load(Ordering::Acquire)
                        != cache.flush_completed.load(Ordering::Acquire)
                },
                Some(Duration::from_secs(5)),
            );

            // A kick arrived while we slept: let the next iteration serve it as
            // a forced pass rather than answering it with this unforced one.
            if cache.flush_requested.load(Ordering::Acquire) != done {
                continue;
            }

            run_pass(cache, false);
            continue;
        }

        // Explicit kick (sync/fsync/flush_device): write everything, then
        // publish the request number so waiters can rely on it.
        //
        // The request is published even when the pass failed, because a waiter
        // that is never answered parks forever. It is said out loud instead:
        // the caller was told its data is durable and some of it is not, which
        // is the shape a corrupt filesystem takes several minutes later.
        if !run_pass(cache, true) {
            cache
                .stats
                .failed_sync_passes
                .fetch_add(1, Ordering::Relaxed);
            log!(
                "writeback: forced pass for request {} did not write every dirty page; \
                 sync is returning without full durability",
                req
            );
        }
        cache.flush_completed.store(req, Ordering::Release);
        cache.sync_done_wq.wake_all();
    }
}
