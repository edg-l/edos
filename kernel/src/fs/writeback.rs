//! Background writeback kthread for the block page cache.
//!
//! Spawned once during boot after AHCI drivers are initialized. Loops forever:
//!   - Waits up to 5 seconds on `writeback_wq` for a kick or periodic tick.
//!   - On wake (or timeout), increments `flush_requested` to treat a timer
//!     expiry as an implicit request, then checks whether there is work to do.
//!   - Calls `flush_dirty_once()` to write all dirty pages in one pass.
//!   - Records the completed request number and wakes `sync_done_wq`.

use core::{sync::atomic::Ordering, time::Duration};

use crate::{fs::block_page_cache::BlockPageCache, log};

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
            // Treat the wakeup (kick or timer expiry) as an implicit request
            // so the loop always runs at least one flush pass.
            cache.flush_requested.fetch_add(1, Ordering::Release);
            continue;
        }

        // Run one flush pass.
        let bytes = match cache.flush_dirty_once() {
            Ok(b) => b,
            Err(e) => {
                log!("writeback: flush error {:?}", e);
                0
            }
        };

        cache.stats.writeback_runs.fetch_add(1, Ordering::Relaxed);
        cache
            .stats
            .writeback_bytes
            .fetch_add(bytes, Ordering::Relaxed);

        if bytes > 0 {
            log!("writeback: flushed {} bytes", bytes);
        }

        // Mark this request as completed and wake any sync_all() waiters.
        cache.flush_completed.store(req, Ordering::Release);
        cache.sync_done_wq.wake_all();
    }
}
