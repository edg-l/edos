//! Background writeback kthread for the block page cache and MAP_SHARED inodes.
//!
//! Spawned once during boot after AHCI drivers are initialized. Loops forever:
//!   - Waits up to 5 seconds on `writeback_wq` for a kick or periodic tick.
//!   - On wake (or timeout), increments `flush_requested` to treat a timer
//!     expiry as an implicit request, then checks whether there is work to do.
//!   - Calls `flush_dirty_once()` to write all dirty pages in one pass.
//!   - Also calls `flush_dirty_inodes()` to flush dirty MAP_SHARED inode pages.
//!   - Records the completed request number and wakes `sync_done_wq`.

use core::{sync::atomic::Ordering, time::Duration};

use crate::{
    fs::{block_page_cache::BlockPageCache, vfs::flush_dirty_inodes},
    log, log_debug,
};

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

            // Re-check: if still req == done, this was a periodic timer wake.
            // Use force=false to respect dirty_expire. If someone kicked while
            // we slept, force=true to honor sync/fsync semantics.
            let new_req = cache.flush_requested.load(Ordering::Acquire);
            let forced = new_req != done;
            if !forced {
                // Periodic timer: bump requested so we run one pass.
                cache.flush_requested.fetch_add(1, Ordering::Release);
            }

            let bytes = match cache.flush_dirty_once(forced) {
                Ok(b) => b,
                Err(e) => {
                    log!("writeback: flush error {:?}", e);
                    0
                }
            };

            // Second pass: flush dirty pages from MAP_SHARED file-backed mappings.
            flush_dirty_inodes();

            cache.stats.writeback_runs.fetch_add(1, Ordering::Relaxed);
            cache
                .stats
                .writeback_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            if bytes > 0 {
                log_debug!("writeback: flushed {} bytes", bytes);
            }

            let completed_req = cache.flush_requested.load(Ordering::Acquire);
            cache
                .flush_completed
                .store(completed_req, Ordering::Release);
            cache.sync_done_wq.wake_all();
            continue;
        }

        // Explicit kick (sync/fsync): force=true, flush everything.
        let bytes = match cache.flush_dirty_once(true) {
            Ok(b) => b,
            Err(e) => {
                log!("writeback: flush error {:?}", e);
                0
            }
        };

        // Second pass: flush dirty MAP_SHARED inode pages.
        flush_dirty_inodes();

        cache.stats.writeback_runs.fetch_add(1, Ordering::Relaxed);
        cache
            .stats
            .writeback_bytes
            .fetch_add(bytes, Ordering::Relaxed);

        if bytes > 0 {
            log_debug!("writeback: flushed {} bytes", bytes);
        }

        // Mark this request as completed and wake any sync_all() waiters.
        cache.flush_completed.store(req, Ordering::Release);
        cache.sync_done_wq.wake_all();
    }
}
