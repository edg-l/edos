use core::{array, cmp};

use alloc::{collections::VecDeque, sync::Arc};

use crate::thread::thread::Thread;

const PRIORITY_LEVELS: usize = 16;
const BOOST_DELTA: usize = 2;
const BOOST_STREAK_LIMIT: usize = 2;

#[derive(Clone)]
struct RunQueueEntry {
    thread: Arc<Thread>,
    boosted: bool,
}

pub(crate) struct RunQueue {
    queues: [VecDeque<RunQueueEntry>; PRIORITY_LEVELS],
    boosted_streak: usize,
}

impl RunQueue {
    pub(crate) fn new() -> Self {
        Self {
            queues: array::from_fn(|_| VecDeque::new()),
            boosted_streak: 0,
        }
    }

    pub(crate) fn enqueue(&mut self, thread: Arc<Thread>, priority: u8, boosted: bool) {
        let base_idx = priority.min((PRIORITY_LEVELS - 1) as u8) as usize;
        let target_idx = if boosted {
            cmp::min(base_idx + BOOST_DELTA, PRIORITY_LEVELS - 1)
        } else {
            base_idx
        };
        let entry = RunQueueEntry {
            boosted: boosted && target_idx != base_idx,
            thread,
        };
        self.queues[target_idx].push_back(entry);
    }

    pub(crate) fn pop_next(&mut self) -> Option<Arc<Thread>> {
        for idx in (0..PRIORITY_LEVELS).rev() {
            if let Some(entry) = self.queues[idx].pop_front() {
                if entry.boosted {
                    self.boosted_streak += 1;
                    if self.boosted_streak > BOOST_STREAK_LIMIT {
                        if let Some(lower) = self.pop_lower_than(idx) {
                            self.queues[idx].push_front(entry);
                            return Some(lower);
                        }
                        self.boosted_streak = BOOST_STREAK_LIMIT;
                    }
                } else {
                    self.boosted_streak = 0;
                }
                return Some(entry.thread);
            }
        }
        self.boosted_streak = 0;
        None
    }

    fn pop_lower_than(&mut self, upper_idx: usize) -> Option<Arc<Thread>> {
        for idx in (0..upper_idx).rev() {
            if let Some(entry) = self.queues[idx].pop_front() {
                if entry.boosted {
                    self.boosted_streak = 1;
                } else {
                    self.boosted_streak = 0;
                }
                return Some(entry.thread);
            }
        }
        None
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queues.iter().all(|q| q.is_empty())
    }
}
