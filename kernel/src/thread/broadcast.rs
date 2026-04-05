use core::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use core::time::Duration;

use alloc::{collections::btree_map::BTreeMap, sync::Arc};
use crossbeam_queue::ArrayQueue;
use spin::RwLock;

use crate::thread::{
    scheduler::{WakePriority, sched},
    thread::{State, ThreadId, get_thread_by_id},
};

/// Default capacity for subscriber queues. Events are dropped when full.
const SUBSCRIBER_QUEUE_CAP: usize = 256;

/// A single subscriber queue (lock-free).
pub struct Subscriber<T> {
    owner: ThreadId,
    queue: ArrayQueue<T>,
}

impl<T> Subscriber<T> {
    pub fn try_recv(&self) -> Option<T> {
        self.queue.pop()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn recv(&self) -> T {
        debug_assert_eq!(
            sched().current_thread_id(),
            Some(self.owner),
            "Subscriber::recv called from non-owner thread"
        );
        loop {
            if let Some(msg) = self.queue.pop() {
                return msg;
            }
            sched().thread_park_while(|| self.queue.is_empty());
        }
    }

    #[allow(dead_code)]
    pub fn recv_timeout(&self, dur: Duration) -> Option<T> {
        debug_assert_eq!(
            sched().current_thread_id(),
            Some(self.owner),
            "Subscriber::recv_timeout called from non-owner thread"
        );

        if let Some(msg) = self.queue.pop() {
            return Some(msg);
        }
        // Sleep until either wake or timeout, then re-check.
        sched().thread_sleep(dur);
        self.queue.pop()
    }
}

/// Broadcaster with many subscribers
pub struct Broadcaster<T> {
    subs: RwLock<BTreeMap<ThreadId, Arc<Subscriber<T>>>>,
    broadcast_count: AtomicU32,
}

impl<T: Clone> Broadcaster<T> {
    pub const fn new() -> Self {
        Self {
            subs: RwLock::new(BTreeMap::new()),
            broadcast_count: AtomicU32::new(0),
        }
    }

    pub fn subscribe(&self) -> Arc<Subscriber<T>> {
        let owner = sched().current_thread_id().unwrap();
        // Single write lock: check-and-insert to avoid TOCTOU race (M7).
        let mut subs = self.subs.write();
        if let Some(existing) = subs.get(&owner) {
            return existing.clone();
        }
        let sub = Arc::new(Subscriber {
            owner,
            queue: ArrayQueue::new(SUBSCRIBER_QUEUE_CAP),
        });
        subs.insert(owner, sub.clone());
        sub
    }

    pub fn unsubscribe(&self) {
        self.subs
            .write()
            .remove(&sched().current_thread_id().unwrap());
    }

    pub fn broadcast(&self, msg: T) {
        let count = self.broadcast_count.fetch_add(1, AtomicOrdering::Relaxed);
        if count > 0 && count % 128 == 0 {
            self.cleanup();
        }
        let sched = sched();
        // Collect into stack-allocated buffer to avoid heap allocation.
        let mut targets: heapless::Vec<Arc<Subscriber<T>>, 32> = heapless::Vec::new();
        {
            let subs = self.subs.read();
            for sub in subs.values() {
                debug_assert!(
                    targets.push(sub.clone()).is_ok(),
                    "broadcast: too many subscribers"
                );
            }
        }
        for sub in &targets {
            // ArrayQueue::push is lock-free; drop event if queue is full.
            let _ = sub.queue.push(msg.clone());
            sched.wake_thread(sub.owner, WakePriority::Normal);
        }
    }

    pub fn broadcast_many(&self, msgs: &[T]) {
        if msgs.is_empty() {
            return;
        }
        let count = self.broadcast_count.fetch_add(1, AtomicOrdering::Relaxed);
        if count > 0 && count % 128 == 0 {
            self.cleanup();
        }
        let sched = sched();
        let mut targets: heapless::Vec<Arc<Subscriber<T>>, 32> = heapless::Vec::new();
        {
            let subs = self.subs.read();
            for sub in subs.values() {
                debug_assert!(
                    targets.push(sub.clone()).is_ok(),
                    "broadcast_many: too many subscribers"
                );
            }
        }
        for sub in &targets {
            for msg in msgs {
                let _ = sub.queue.push(msg.clone());
            }
            sched.wake_thread(sub.owner, WakePriority::Normal);
        }
    }

    pub fn cleanup(&self) {
        // Single write lock with retain avoids heap allocation.
        let mut subs = self.subs.write();
        subs.retain(|_, sub| {
            get_thread_by_id(sub.owner).is_some_and(|t| t.state() != State::Dying)
        });
    }
}
