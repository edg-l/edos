// Journal committer kthread.
//
// Loops forever, waiting up to 1 second for a kick or a periodic tick, then
// drives seal_and_commit on every registered journal.  After each successful
// commit it advances the tail (reclaims ring space).

use core::time::Duration;

use crate::{fs::block_page_cache::BlockPageCache, log};

pub fn committer_thread() -> ! {
    loop {
        let cache = BlockPageCache::global();

        // Wait up to 1 second for a kick from force_commit_and_wait or a
        // periodic flush tick.  We use commit_kick_wq of any single journal
        // (or just sleep) since we iterate all journals anyway.
        let journals = cache.all_journals();

        if journals.is_empty() {
            // No EFS volumes mounted yet; sleep briefly and retry.
            let dummy_wq = crate::thread::waitqueue::WaitQueue::new();
            dummy_wq.wait_until_timeout(|| false, Some(Duration::from_millis(200)));
        } else {
            // Wait on the first journal's kick queue with a 1-second timeout,
            // then process all journals.
            let wq = &journals[0].commit_kick_wq;
            wq.wait_until_timeout(
                || journals.iter().any(|j| j.has_pending_work()),
                Some(Duration::from_secs(1)),
            );
        }

        // Re-fetch journals (may have changed while we slept).
        let journals = cache.all_journals();

        for j in &journals {
            // Always try to advance the tail first — even if the last commit
            // failed due to ring-full, draining checkpoints frees space.
            if let Err(e) = j.advance_tail() {
                log!("journal_committer: advance_tail error: {:?}", e);
            }
            if j.has_pending_work() {
                if let Err(e) = j.seal_and_commit_if_needed() {
                    log!("journal_committer: seal_and_commit error: {:?}", e);
                }
            }
        }
    }
}
