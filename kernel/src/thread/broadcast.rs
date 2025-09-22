use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use alloc::{
    collections::{btree_map::BTreeMap, vec_deque::VecDeque},
    sync::Arc,
    vec::Vec,
};
use crossbeam_queue::SegQueue;
use spin::Mutex;
use thiserror::Error;
use x86_64::instructions::interrupts::without_interrupts;

use crate::{
    thread::{scheduler::sched, thread::ThreadId},
    timer::Instant,
};

/// A single subscriber queue
pub struct Subscriber<T> {
    owner: ThreadId,
    queue: Mutex<VecDeque<T>>,
}

impl<T> Subscriber<T> {
    pub fn recv(&self) -> T {
        let sched = sched();
        loop {
            if let Some(msg) = self.queue.lock().pop_front() {
                return msg;
            }
            sched.park_thread(sched.current_thread_id().unwrap());
        }
    }

    pub fn recv_timeout(&self, dur: Duration) -> Option<T> {
        let deadline = Instant::now() + dur; // adapt to your tick granularity
        let sched = sched();

        if let Some(msg) = self.queue.lock().pop_front() {
            return Some(msg);
        }
        // park until either wake or timeout
        sched.sleep_for_ticks(sched.current_thread_id().unwrap(), deadline.tick());
        return self.queue.lock().pop_front();
    }
}

/// Broadcaster with many subscribers
pub struct Broadcaster<T> {
    subs: Mutex<Vec<Arc<Subscriber<T>>>>,
}

impl<T: Clone> Broadcaster<T> {
    pub fn new() -> Self {
        Self {
            subs: Mutex::new(Vec::new()),
        }
    }

    pub fn subscribe(&self, owner: ThreadId) -> Arc<Subscriber<T>> {
        let sub = Arc::new(Subscriber {
            owner,
            queue: Mutex::new(VecDeque::new()),
        });
        self.subs.lock().push(sub.clone());
        sub
    }

    pub fn broadcast(&self, msg: T) {
        let sched = sched();
        let subs = self.subs.lock();
        for sub in subs.iter() {
            {
                let mut q = sub.queue.lock();
                q.push_back(msg.clone());
            }
            sched.wake_thread(sub.owner, false);
        }
    }
}
