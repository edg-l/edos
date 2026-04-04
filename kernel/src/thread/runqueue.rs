use core::{array, cmp, sync::atomic::Ordering};

use alloc::sync::Arc;
use intrusive_list::IntrusiveList;

use crate::thread::thread::{State, Thread};

pub const PRIORITY_LEVELS: usize = 16;
const BOOST_DELTA: usize = 2;
const BOOST_STREAK_LIMIT: usize = 2;

pub const DEFAULT_PRIORITY: u8 = 7;
pub const IO_PRIORITY: u8 = 8;

pub(crate) struct RunQueue {
    queues: [IntrusiveList<Thread>; PRIORITY_LEVELS],
    boosted_streak: usize,
}

impl RunQueue {
    pub(crate) fn new() -> Self {
        Self {
            queues: array::from_fn(|_| IntrusiveList::new()),
            boosted_streak: 0,
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
        thread
            .rq_boosted
            .store(boosted && target_idx != base_idx, Ordering::Relaxed);

        let ptr = Arc::into_raw(thread) as *mut Thread;
        unsafe { self.queues[target_idx].push_back(ptr) };
    }

    /// Pop the highest-priority thread. Returns an Arc (reclaims the refcount).
    pub(crate) fn pop_next(&mut self) -> Option<Arc<Thread>> {
        for idx in (0..PRIORITY_LEVELS).rev() {
            if let Some(ptr) = self.queues[idx].pop_front() {
                let thread = unsafe { Arc::from_raw(ptr) };
                debug_assert!(
                    !thread.rq_link.is_linked(),
                    "runqueue::pop_next: thread {} still linked after pop",
                    thread.id.0
                );
                let boosted = thread.rq_boosted.load(Ordering::Relaxed);
                if boosted {
                    self.boosted_streak += 1;
                    if self.boosted_streak > BOOST_STREAK_LIMIT {
                        if let Some(lower) = self.pop_lower_than(idx) {
                            // Re-enqueue the boosted thread at the front.
                            let requeue_ptr = Arc::into_raw(thread) as *mut Thread;
                            unsafe { self.queues[idx].push_front(requeue_ptr) };
                            return Some(lower);
                        }
                        self.boosted_streak = BOOST_STREAK_LIMIT;
                    }
                } else {
                    self.boosted_streak = 0;
                }
                return Some(thread);
            }
        }
        self.boosted_streak = 0;
        None
    }

    fn pop_lower_than(&mut self, upper_idx: usize) -> Option<Arc<Thread>> {
        for idx in (0..upper_idx).rev() {
            if let Some(ptr) = self.queues[idx].pop_front() {
                let thread = unsafe { Arc::from_raw(ptr) };
                let boosted = thread.rq_boosted.load(Ordering::Relaxed);
                if boosted {
                    self.boosted_streak = 1;
                } else {
                    self.boosted_streak = 0;
                }
                return Some(thread);
            }
        }
        None
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queues.iter().all(|q| q.is_empty())
    }
}
