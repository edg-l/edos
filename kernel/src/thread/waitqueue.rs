use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use alloc::{collections::vec_deque::VecDeque, sync::Arc, vec::Vec};
use spin::Mutex;

use crate::{
    log,
    thread::{scheduler::sched, thread::ThreadId},
};

#[repr(u8)]
enum WState {
    Queued = 0,
    Sleeping = 1,
    Woken = 2,
    Cancelled = 3,
}

#[derive(Debug)]
struct WaitEntry {
    tid: ThreadId,
    state: AtomicU8,
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

    /// Wait until `ready()` returns true. Must be called in a loop at call site.
    ///
    /// Caller should call this function in a loop, rechecking themselves ready().
    ///
    /// while !cond() { wait_until(cond); }
    ///
    pub fn wait_until<F: Fn() -> bool>(&self, ready: F) {
        if ready() {
            return;
        }

        let w = Arc::new(WaitEntry {
            tid: sched().current_thread_id().unwrap(),
            state: AtomicU8::new(WState::Queued as u8),
        });
        {
            let mut q = self.inner.lock();
            q.push_back(w.clone());
        }

        // Recheck after enqueuing. If already satisfied or woken, do not sleep.
        if ready() {
            // Try to cancel our token so waker ignores it.
            if w.state.swap(WState::Cancelled as u8, Ordering::AcqRel) == WState::Woken as u8 {
                // was woken concurrently; proceed
            }
            return;
        }

        // Try to transition to Sleeping. If we lose, someone already woke us.
        if w.state
            .compare_exchange(
                WState::Queued as u8,
                WState::Sleeping as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            // Park current thread; scheduler must return only after a wake.
            sched().thread_park();
        }

        // Pair with waker's Release before consuming any guarded state.
        let _ = w.state.load(Ordering::Acquire);
    }

    /// Wake one thread
    ///
    /// Returns true if a thread was woken.
    pub fn wake_one(&self) -> bool {
        let entry = loop {
            let mut q = self.inner.lock();
            if let Some(e) = q.pop_front() {
                if e.state.load(Ordering::Acquire) != WState::Cancelled as u8 {
                    break Some(e);
                }
                continue; // skip cancelled, loop again
            } else {
                break None;
            }
        };
        if let Some(e) = entry {
            let prev = e.state.swap(WState::Woken as u8, Ordering::Release);
            if prev == WState::Sleeping as u8 {
                sched().wake_thread(e.tid, false);
                return true;
            }
        }
        false
    }

    /// Wake all threads
    ///
    /// Returns threads awakened.
    pub fn wake_all(&self) -> usize {
        let list = {
            let mut q = self.inner.lock();
            q.drain(..).collect::<Vec<_>>()
        };
        let mut n = 0;
        for e in list {
            let prev = e.state.swap(WState::Woken as u8, Ordering::Release);
            if prev == WState::Sleeping as u8 {
                sched().wake_thread(e.tid, false);
                n += 1;
            }
        }
        n
    }
}
