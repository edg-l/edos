use alloc::{collections::vec_deque::VecDeque, vec::Vec};
use spin::Mutex;
use x86_64::instructions::interrupts::{self, without_interrupts};

use crate::thread::{scheduler::sched, thread::ThreadId};

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
            }

            if ready() {
                let mut q = self.inner.lock();
                if let Some(pos) = q.iter().position(|&id| id == tid) {
                    q.remove(pos);
                }
                return;
            }

            // Park current thread; it will be woken by wake_one/wake_all
            sched().thread_park();
        });
    }

    /// Wake one thread
    pub fn wake_one(&self) -> bool {
        without_interrupts(|| {
            let tid_opt = {
                let mut q = self.inner.lock();
                q.pop_front()
            };
            if let Some(tid) = tid_opt {
                sched().wake_thread(tid, false);
                true
            } else {
                false
            }
        })
    }

    /// Wake all threads
    pub fn wake_all(&self) -> usize {
        without_interrupts(|| {
            let tids = {
                let mut q = self.inner.lock();
                q.drain(..).collect::<Vec<_>>()
            };
            let n = tids.len();
            for tid in tids {
                sched().wake_thread(tid, false);
            }
            n
        })
    }
}
