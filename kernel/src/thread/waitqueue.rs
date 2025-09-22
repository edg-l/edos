use core::sync::atomic::AtomicBool;

use alloc::{collections::vec_deque::VecDeque, sync::Arc};
use spin::Mutex;

use crate::thread::threadv2::ThreadId;



struct WaitEntry {
    tid: ThreadId,
    woken: AtomicBool,
}

pub struct WaitQueue {
    inner: Mutex<VecDeque<Arc<WaitEntry>>>,
}

impl WaitQueue {
    pub fn new() -> Self {
        Self { inner: Mutex::new(VecDeque::new()) }
    }

    /// Current thread goes to sleep until woken
    pub fn wait(&self) {
        let tid = current_thread_id(); // your kernel helper
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
            thread_park();

            if entry.woken.swap(false, Acquire) {
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
            entry.woken.store(true, Release);
            thread_wake(entry.tid, false); // false = normal priority
        }
    }

    /// Wake all threads
    pub fn wake_all(&self) {
        let list = {
            let mut q = self.inner.lock();
            q.drain(..).collect::<Vec<_>>()
        };

        for entry in list {
            entry.woken.store(true, Release);
            thread_wake(entry.tid, false);
        }
    }
}
