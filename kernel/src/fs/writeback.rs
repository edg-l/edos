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
fn run_pass(cache: &BlockPageCache, force: bool) -> u64 {
    let mut bytes = flush_blocks(cache, force);
    flush_dirty_inodes();
    bytes += flush_blocks(cache, force);

    cache.stats.writeback_runs.fetch_add(1, Ordering::Relaxed);
    cache
        .stats
        .writeback_bytes
        .fetch_add(bytes, Ordering::Relaxed);
    if bytes > 0 {
        log_debug!("writeback: flushed {} bytes", bytes);
    }
    bytes
}

fn flush_blocks(cache: &BlockPageCache, force: bool) -> u64 {
    match cache.flush_dirty_once(force) {
        Ok(b) => b,
        Err(e) => {
            log!("writeback: flush error {:?}", e);
            0
        }
    }
}

pub fn writeback_thread() -> ! {
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
        run_pass(cache, true);
        cache.flush_completed.store(req, Ordering::Release);
        cache.sync_done_wq.wake_all();
    }
}
