use core::sync::atomic::{AtomicBool, Ordering};

use crate::thread::{
    scheduler::{WakePriority, sched},
    thread::ThreadId,
};

#[derive(Debug)]
pub struct PollWaiter {
    thread: ThreadId,
    pending: AtomicBool,
}

impl PollWaiter {
    pub fn new(thread: ThreadId) -> Self {
        Self {
            thread,
            pending: AtomicBool::new(false),
        }
    }

    /// Notify the owning thread that an event has been delivered.
    pub fn notify(&self) {
        self.pending.store(true, Ordering::Release);
        sched().wake_thread(self.thread, WakePriority::Normal);
    }

    /// Clear the pending flag, returning whether a notification was pending.
    pub fn arm(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
}
