// Journal committer kthread.
//
// Waits on the one committer waitqueue for a kick, a newly registered journal
// or the periodic tick, then drives seal_and_commit on every registered
// journal. After each successful commit it advances the tail (reclaims ring
// space).
//
// Commits run here and nowhere else: `seal_and_commit` reads the ring head,
// releases the state lock and only then writes, so two threads committing one
// journal would claim the same ring position. `force_commit_and_wait` kicks
// and waits rather than committing inline for that reason, which makes this
// thread's wakeup latency that caller's latency.

use core::{sync::atomic::Ordering, time::Duration};

use crate::{
    fs::{
        block_page_cache::BlockPageCache,
        journal::{COMMITTER_WAKE, COMMITTER_WQ},
    },
    log,
};

/// Periodic commit interval, matching the jbd2 default. A kick or a mount
/// makes the predicate true and returns well before it.
const COMMIT_INTERVAL: Duration = Duration::from_secs(5);

pub fn committer_thread() -> ! {
    loop {
        let cache = BlockPageCache::global();

        // Read the generation before the journal list: a registration landing
        // between the two changes the generation, so the wait returns at once
        // rather than missing the journal it has just gained.
        let seen = COMMITTER_WAKE.load(Ordering::Acquire);
        let journals = cache.all_journals();

        COMMITTER_WQ.wait_until_timeout(
            || {
                COMMITTER_WAKE.load(Ordering::Acquire) != seen
                    || journals.iter().any(|j| j.has_pending_work_hint())
            },
            Some(COMMIT_INTERVAL),
        );

        // Re-fetch journals (may have changed while we slept).
        let journals = cache.all_journals();

        for j in &journals {
            // Always try to advance the tail first — even if the last commit
            // failed due to ring-full, draining checkpoints frees space.
            if let Err(e) = j.advance_tail() {
                log!("journal_committer: advance_tail error: {:?}", e);
            }
            if j.has_pending_work()
                && let Err(e) = j.seal_and_commit_if_needed()
            {
                log!("journal_committer: seal_and_commit error: {:?}", e);
            }
        }
    }
}
