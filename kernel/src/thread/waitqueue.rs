use core::time::Duration;
use heapless::Deque;
use spin::Mutex;
use x86_64::instructions::interrupts::{self, without_interrupts};

use crate::thread::{
    scheduler::{WakePriority, sched},
    thread::ThreadId,
};

/// Maximum number of threads that can wait on a single WaitQueue.
const WAITQUEUE_CAP: usize = 32;

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
    inner: Mutex<Deque<ThreadId, WAITQUEUE_CAP>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(Deque::new()),
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
    pub fn wake_all(&self) -> usize {
        // Drain into stack buffer under lock, then wake outside to avoid
        // holding the lock while wake_thread_slow spins.
        let tids: heapless::Vec<ThreadId, WAITQUEUE_CAP> = without_interrupts(|| {
            let mut q = self.inner.lock();
            let mut v = heapless::Vec::new();
            while let Some(tid) = q.pop_front() {
                let _ = v.push(tid);
            }
            v
        });
        let n = tids.len();
        for tid in tids {
            sched().wake_thread(tid, WakePriority::Normal);
        }
        n
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

        // Enqueue and check readiness inside without_interrupts to close the
        // lost-wakeup window: heapless::Deque does not allocate, so this is safe.
        // Park/sleep must happen OUTSIDE without_interrupts since context switching
        // requires interrupts to be re-enabled.
        interrupts::without_interrupts(|| {
            {
                let mut q = self.inner.lock();
                q.push_back(tid)
                    .expect("WaitQueue overflow: too many waiters");
            }

            if ready() {
                let mut q = self.inner.lock();
                q.retain(|&id| id != tid);
                return;
            }

            action = Some(match timeout {
                Some(dt) => SleepAction::Sleep(dt),
                None => SleepAction::Park,
            });
        });

        // Perform the actual park/sleep with interrupts enabled.
        // thread_park_while sets Parked before checking the closure, so a waker
        // that fires after IRQs re-enable but before park will still succeed.
        if let Some(chosen) = action {
            match chosen {
                SleepAction::Park => {
                    sched().thread_park_while(|| !ready());
                }
                SleepAction::Sleep(dt) => {
                    sched().thread_sleep(dt);
                }
            }
        }

        // Always remove our tid from the wait queue after waking,
        // regardless of how we were woken (park, sleep, or timeout).
        interrupts::without_interrupts(|| {
            let mut q = self.inner.lock();
            q.retain(|&id| id != tid);
        });

        let Some(action) = action else {
            return WaitOutcome::Ready;
        };

        if ready() {
            return WaitOutcome::Parked;
        }

        match action {
            SleepAction::Park => WaitOutcome::Parked,
            SleepAction::Sleep(_) => WaitOutcome::TimedOut,
        }
    }
}
