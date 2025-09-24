use alloc::{collections::vec_deque::VecDeque, vec::Vec};
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::{
    log,
    thread::{scheduler::sched, thread::ThreadId},
};

#[derive(Debug)]
pub struct WaitQueue {
    inner: Mutex<VecDeque<ThreadId>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
        }
    }

    /// Put the current thread to sleep until woken.
    pub fn wait_until<F: Fn() -> bool>(&self, ready: F) {
        if ready() {
            return;
        }

        let tid = sched().current_thread_id().unwrap();
        interrupts::without_interrupts(|| {
            if ready() {
                return;
            }

            {
                let mut q = self.inner.lock();
                q.push_back(tid);
                log!("waitqueue enqueue tid={} len={}", tid.0, q.len());
            }

            if ready() {
                let mut q = self.inner.lock();
                if let Some(pos) = q.iter().position(|&id| id == tid) {
                    q.remove(pos);
                    log!(
                        "waitqueue dequeue-before-park tid={} len={}",
                        tid.0,
                        q.len()
                    );
                }
                return;
            }

            // Park current thread; it will be woken by wake_one/wake_all
            sched().thread_park();
            log!("waitqueue wakeup tid={}", tid.0);
        });
    }

    /// Wake one thread
    pub fn wake_one(&self) -> bool {
        let tid_opt = {
            let mut q = self.inner.lock();
            q.pop_front()
        };
        if let Some(tid) = tid_opt {
            log!("waitqueue wake_one tid={}", tid.0);
            sched().wake_thread(tid, false);
            true
        } else {
            false
        }
    }

    /// Wake all threads
    pub fn wake_all(&self) -> usize {
        let tids = {
            let mut q = self.inner.lock();
            q.drain(..).collect::<Vec<_>>()
        };
        log!("waitqueue wake_all count={}", tids.len());
        let n = tids.len();
        for tid in tids {
            sched().wake_thread(tid, false);
        }
        n
    }
}
