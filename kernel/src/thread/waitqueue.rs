use core::sync::atomic::{AtomicBool, Ordering};

use alloc::{collections::vec_deque::VecDeque, sync::Arc, vec::Vec};
use spin::Mutex;

use crate::thread::{scheduler::sched, thread::ThreadId};

#[derive(Debug)]
struct WaitEntry {
    tid: ThreadId,
    woken: AtomicBool,
}

#[derive(Debug)]
pub struct WaitQueue {
    inner: Mutex<VecDeque<Arc<WaitEntry>>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
        }
    }

    /// Current thread goes to sleep until woken
    pub fn wait(&self) {
        let tid = sched().current_thread_id().unwrap(); // your kernel helper
        let entry = Arc::new(WaitEntry {
            tid,
            woken: AtomicBool::new(false),
        });

        {
            let mut q = self.inner.lock();
            q.push_back(entry.clone());
        }

        // park until woken
        loop {
            // Scheduler call to block this thread
            sched().park_thread(tid);

            if entry.woken.swap(false, Ordering::Acquire) {
                break;
            }
        }
    }

    /// Wake one thread
    pub fn wake_one(&self) {
        let opt = {
            let mut q = self.inner.lock();
            q.pop_front()
        };

        if let Some(entry) = opt {
            entry.woken.store(true, Ordering::Release);
            sched().wake_thread(entry.tid, false);
        }
    }

    /// Wake all threads
    pub fn wake_all(&self) {
        let list = {
            let mut q = self.inner.lock();
            q.drain(..).collect::<Vec<_>>()
        };

        for entry in list {
            entry.woken.store(true, Ordering::Release);
            sched().wake_thread(entry.tid, false);
        }
    }
}
