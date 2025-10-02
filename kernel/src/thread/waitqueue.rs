use alloc::{collections::vec_deque::VecDeque, vec::Vec};
use core::time::Duration;
use spin::Mutex;
use x86_64::instructions::interrupts::{self, without_interrupts};

use crate::thread::{
    scheduler::{WakePriority, sched},
    thread::ThreadId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Condition became ready without blocking.
    Ready,
    /// Thread was parked and later woken.
    Parked,
    /// Thread slept until the timeout elapsed without the condition becoming ready.
    TimedOut,
}

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
    pub fn wait_until<F: Fn() -> bool>(&self, ready: F) -> WaitOutcome {
        self.wait_internal(ready, None)
    }

    /// Put the current thread to sleep until woken or the timeout elapses.
    pub fn wait_until_timeout<F: Fn() -> bool>(
        &self,
        ready: F,
        timeout: Option<Duration>,
    ) -> WaitOutcome {
        self.wait_internal(ready, timeout)
    }

    /// Wake one thread
    pub fn wake_one(&self) -> bool {
        without_interrupts(|| {
            let tid_opt = {
                let mut q = self.inner.lock();
                q.pop_front()
            };
            if let Some(tid) = tid_opt {
                sched().wake_thread(tid, WakePriority::Normal);
                true
            } else {
                false
            }
        })
    }

    /// Wake all threads
    #[expect(unused)]
    pub fn wake_all(&self) -> usize {
        without_interrupts(|| {
            let tids = {
                let mut q = self.inner.lock();
                q.drain(..).collect::<Vec<_>>()
            };
            let n = tids.len();
            for tid in tids {
                sched().wake_thread(tid, WakePriority::Normal);
            }
            n
        })
    }

    /// Check whether the queue currently has any waiters.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    fn wait_internal<F: Fn() -> bool>(&self, ready: F, timeout: Option<Duration>) -> WaitOutcome {
        if ready() {
            return WaitOutcome::Ready;
        }

        #[derive(Copy, Clone)]
        enum SleepAction {
            Park,
            Sleep(Duration),
        }

        let tid = sched().current_thread_id().unwrap();
        let mut action: Option<SleepAction> = None;

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

            let chosen = match timeout {
                Some(dt) => SleepAction::Sleep(dt),
                None => SleepAction::Park,
            };

            action = Some(chosen);

            match chosen {
                SleepAction::Park => {
                    sched().thread_park();
                }
                SleepAction::Sleep(dt) => {
                    sched().thread_sleep(dt);
                }
            }
        });

        let Some(action) = action else {
            return WaitOutcome::Ready;
        };

        if ready() {
            return WaitOutcome::Parked;
        }

        match action {
            SleepAction::Park => WaitOutcome::Parked,
            SleepAction::Sleep(_) => {
                interrupts::without_interrupts(|| {
                    let mut q = self.inner.lock();
                    if let Some(pos) = q.iter().position(|&id| id == tid) {
                        q.remove(pos);
                    }
                });
                WaitOutcome::TimedOut
            }
        }
    }
}
