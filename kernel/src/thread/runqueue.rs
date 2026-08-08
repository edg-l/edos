use core::{array, cmp};

use alloc::sync::Arc;
use intrusive_list::IntrusiveList;

use crate::thread::thread::{State, Thread};

pub const PRIORITY_LEVELS: usize = 16;
const BOOST_DELTA: usize = 2;

/// Consecutive picks from one level before the highest non-empty lower level
/// is serviced once.
///
/// Strict priority order alone lets a runnable thread be passed over forever,
/// which turns any spin lock shared across priorities into a deadlock: the
/// holder is `Ready` and never picked, while the higher-priority spinner never
/// makes progress. This bounds that inversion to `STARVE_STREAK_LIMIT` picks.
const STARVE_STREAK_LIMIT: usize = 2;

pub const DEFAULT_PRIORITY: u8 = 7;
pub const IO_PRIORITY: u8 = 8;

pub(crate) struct RunQueue {
    queues: [IntrusiveList<Thread>; PRIORITY_LEVELS],
    starve_streak: usize,
}

impl RunQueue {
    pub(crate) fn new() -> Self {
        Self {
            queues: array::from_fn(|_| IntrusiveList::new()),
            starve_streak: 0,
        }
    }

    /// Enqueue a thread. Consumes one Arc refcount into the list.
    pub(crate) fn enqueue(&mut self, thread: Arc<Thread>, priority: u8, boosted: bool) {
        debug_assert!(
            !thread.rq_link.is_linked(),
            "runqueue::enqueue: thread {} already linked",
            thread.id.0
        );
        debug_assert!(
            thread.state() == State::Ready,
            "runqueue::enqueue: thread {} state {:?}, expected Ready",
            thread.id.0,
            thread.state()
        );

        let base_idx = priority.min((PRIORITY_LEVELS - 1) as u8) as usize;
        let target_idx = if boosted {
            cmp::min(base_idx + BOOST_DELTA, PRIORITY_LEVELS - 1)
        } else {
            base_idx
        };

        let ptr = Arc::into_raw(thread) as *mut Thread;
        unsafe { self.queues[target_idx].push_back(ptr) };
    }

    /// Pop the highest-priority thread. Returns an Arc (reclaims the refcount).
    ///
    /// Every `STARVE_STREAK_LIMIT` consecutive picks, the highest non-empty
    /// lower level is serviced instead, so a runnable thread behind a busy
    /// higher-priority one is delayed but never passed over indefinitely.
    pub(crate) fn pop_next(&mut self) -> Option<Arc<Thread>> {
        for idx in (0..PRIORITY_LEVELS).rev() {
            if let Some(ptr) = self.queues[idx].pop_front() {
                let thread = unsafe { Arc::from_raw(ptr) };
                debug_assert!(
                    !thread.rq_link.is_linked(),
                    "runqueue::pop_next: thread {} still linked after pop",
                    thread.id.0
                );

                self.starve_streak += 1;
                if self.starve_streak > STARVE_STREAK_LIMIT {
                    if let Some(lower) = self.pop_lower_than(idx) {
                        // Put the passed-over thread back at the head of its
                        // level so it is next in line there.
                        let requeue_ptr = Arc::into_raw(thread) as *mut Thread;
                        unsafe { self.queues[idx].push_front(requeue_ptr) };
                        return Some(lower);
                    }
                    // Nothing lower to service; stay at the limit so the next
                    // pick checks again.
                    self.starve_streak = STARVE_STREAK_LIMIT;
                }
                return Some(thread);
            }
        }
        self.starve_streak = 0;
        None
    }

    fn pop_lower_than(&mut self, upper_idx: usize) -> Option<Arc<Thread>> {
        for idx in (0..upper_idx).rev() {
            if let Some(ptr) = self.queues[idx].pop_front() {
                let thread = unsafe { Arc::from_raw(ptr) };
                debug_assert!(
                    !thread.rq_link.is_linked(),
                    "runqueue::pop_lower_than: thread {} still linked after pop",
                    thread.id.0
                );
                self.starve_streak = 0;
                return Some(thread);
            }
        }
        None
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queues.iter().all(|q| q.is_empty())
    }

    pub(crate) fn total_len(&self) -> usize {
        self.queues.iter().map(|q| q.len()).sum()
    }

    /// Pop the lowest-priority thread from the back of the queue.
    /// Used by work-stealing: take the least important thread to minimize
    /// impact on the victim CPU.
    pub(crate) fn pop_back_any(&mut self) -> Option<Arc<Thread>> {
        for idx in 0..PRIORITY_LEVELS {
            if let Some(ptr) = self.queues[idx].pop_back() {
                let thread = unsafe { Arc::from_raw(ptr) };
                debug_assert!(
                    !thread.rq_link.is_linked(),
                    "runqueue::pop_back_any: thread {} still linked after pop",
                    thread.id.0
                );
                // A steal disrupts the queue order, so the streak no longer
                // describes what this queue has been servicing.
                self.starve_streak = 0;
                return Some(thread);
            }
        }
        None
    }
}
