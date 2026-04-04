use core::time::Duration;

use alloc::{
    collections::{btree_map::BTreeMap, vec_deque::VecDeque},
    sync::Arc,
    vec::Vec,
};
use spin::RwLock;

use crate::thread::{
    mutex::BlockingMutex,
    scheduler::{WakePriority, sched},
    thread::{State, ThreadId, get_thread_by_id},
};

/// A single subscriber queue
pub struct Subscriber<T> {
    owner: ThreadId,
    queue: BlockingMutex<VecDeque<T>>,
}

impl<T> Subscriber<T> {
    pub fn try_recv(&self) -> Option<T> {
        self.queue.lock().pop_front()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }

    pub fn recv(&self) -> T {
        debug_assert_eq!(
            sched().current_thread_id(),
            Some(self.owner),
            "Subscriber::recv called from non-owner thread"
        );
        loop {
            if let Some(msg) = self.queue.lock().pop_front() {
                return msg;
            }
            sched().thread_park_while(|| self.queue.lock().is_empty());
        }
    }

    #[allow(dead_code)]
    pub fn recv_timeout(&self, dur: Duration) -> Option<T> {
        debug_assert_eq!(
            sched().current_thread_id(),
            Some(self.owner),
            "Subscriber::recv_timeout called from non-owner thread"
        );

        if let Some(msg) = self.queue.lock().pop_front() {
            return Some(msg);
        }
        // Sleep until either wake or timeout, then re-check.
        sched().thread_sleep(dur);
        self.queue.lock().pop_front()
    }
}

/// Broadcaster with many subscribers
pub struct Broadcaster<T> {
    subs: RwLock<BTreeMap<ThreadId, Arc<Subscriber<T>>>>,
}

impl<T: Clone> Broadcaster<T> {
    pub const fn new() -> Self {
        Self {
            subs: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn subscribe(&self) -> Arc<Subscriber<T>> {
        let owner = sched().current_thread_id().unwrap();
        {
            let subs = self.subs.read();
            if let Some(existing) = subs.get(&owner) {
                return existing.clone();
            }
        }

        let sub = Arc::new(Subscriber {
            owner,
            queue: BlockingMutex::new(VecDeque::new()),
        });
        self.subs.write().insert(owner, sub.clone());
        sub
    }

    pub fn unsubscribe(&self) {
        self.subs
            .write()
            .remove(&sched().current_thread_id().unwrap());
    }

    pub fn broadcast(&self, msg: T) {
        let sched = sched();
        let targets: Vec<Arc<Subscriber<T>>> = {
            let subs = self.subs.read();
            subs.values().cloned().collect()
        };
        for sub in targets {
            {
                let mut q = sub.queue.lock();
                q.push_back(msg.clone());
            }
            sched.wake_thread(sub.owner, WakePriority::Normal);
        }
    }

    pub fn broadcast_many(&self, msgs: &[T]) {
        if msgs.is_empty() {
            return;
        }

        let sched = sched();
        let targets: Vec<Arc<Subscriber<T>>> = {
            let subs = self.subs.read();
            subs.values().cloned().collect()
        };
        for sub in targets {
            {
                let mut q = sub.queue.lock();
                for msg in msgs {
                    q.push_back(msg.clone());
                }
            }
            sched.wake_thread(sub.owner, WakePriority::Normal);
        }
    }

    pub fn cleanup(&self) {
        let to_remove = {
            let subs = self.subs.read();

            if subs.is_empty() {
                return;
            }

            let mut to_remove = Vec::new();
            for sub in subs.iter() {
                if let Some(thread) = get_thread_by_id(*sub.0) {
                    if thread.state() == State::Dying {
                        to_remove.push(*sub.0);
                    }
                } else {
                    to_remove.push(*sub.0);
                }
            }
            to_remove
        };

        // Actually remove dead subscribers
        if !to_remove.is_empty() {
            let mut subs = self.subs.write();
            for tid in to_remove {
                subs.remove(&tid);
            }
        }
    }
}
