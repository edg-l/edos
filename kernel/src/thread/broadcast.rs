use core::time::Duration;

use alloc::{
    collections::{btree_map::BTreeMap, vec_deque::VecDeque},
    sync::Arc,
    vec::Vec,
};
use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;

use crate::{
    fs::PollState,
    thread::{scheduler::sched, thread::ThreadId},
};

/// A single subscriber queue
pub struct Subscriber<T> {
    owner: ThreadId,
    queue: Mutex<VecDeque<T>>,
}

impl<T> Subscriber<T> {
    pub fn try_recv(&self) -> Option<T> {
        self.queue.lock().pop_front()
    }

    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }

    pub fn recv(&self) -> T {
        let sched = sched();
        loop {
            if let Some(msg) = self.queue.lock().pop_front() {
                return msg;
            }
            sched.thread_park();
        }
    }

    pub fn recv_timeout(&self, dur: Duration) -> Option<T> {
        let sched = sched();

        if let Some(msg) = self.queue.lock().pop_front() {
            return Some(msg);
        }
        // park until either wake or timeout
        sched.thread_sleep(dur);
        return self.queue.lock().pop_front();
    }

    pub fn poll(&self, dur: Duration) -> PollState {
        let sched = sched();

        if !self.queue.lock().is_empty() {
            return PollState {
                readable: true,
                writable: true,
                error: false,
            };
        }

        sched.thread_sleep(dur);

        return PollState {
            readable: !self.queue.lock().is_empty(),
            writable: true,
            error: false,
        };
    }
}

/// Broadcaster with many subscribers
pub struct Broadcaster<T> {
    subs: Mutex<BTreeMap<ThreadId, Arc<Subscriber<T>>>>,
}

impl<T: Clone> Broadcaster<T> {
    pub const fn new() -> Self {
        Self {
            subs: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn subscribe(&self) -> Arc<Subscriber<T>> {
        let mut subs = self.subs.lock();
        let owner = sched().current_thread_id().unwrap();
        if let Some(existing) = subs.get(&owner) {
            return existing.clone();
        }

        let sub = Arc::new(Subscriber {
            owner,
            queue: Mutex::new(VecDeque::new()),
        });
        subs.insert(owner, sub.clone());
        sub
    }

    pub fn unsubscribe(&self) {
        self.subs
            .lock()
            .remove(&sched().current_thread_id().unwrap());
    }

    pub fn broadcast(&self, msg: T) {
        let sched = sched();
        let targets: Vec<Arc<Subscriber<T>>> = {
            let subs = self.subs.lock();
            subs.values().cloned().collect()
        };
        for sub in targets {
            {
                let mut q = sub.queue.lock();
                q.push_back(msg.clone());
            }
            sched.wake_thread(sub.owner, false);
        }
    }
}
