use core::time::Duration;

use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use crossbeam_queue::SegQueue;
use spin::{RwLock};
use thiserror::Error;

use crate::thread::{ThreadId, scheduler::sched};

#[derive(Debug)]
pub struct Broadcast<T: Clone> {
    subscribers: RwLock<BTreeMap<ThreadId, Receiver<T>>>,
    history: RwLock<Vec<T>>,
    send_history: bool,
    bound: usize,
}

impl<T: Clone> Broadcast<T> {
    /// If send_history is true when a new subscriber will get all history sent.
    pub const fn new(bound: usize, send_history: bool) -> Self {
        Self {
            subscribers: RwLock::new(BTreeMap::new()),
            history: RwLock::new(Vec::new()),
            send_history,
            bound,
        }
    }

    /// The calling thread subscribes.
    pub fn subscribe_or_get(&self) -> Receiver<T> {
        if let Some(r) = self.subscribers.read().get(&sched().current_id()) {
            return r.clone();
        }

        let mut subs = self.subscribers.write();
        let tid = sched().current_id();
        let rx = (*subs.entry(tid.clone()).or_default()).clone();

        if self.send_history {
            for x in self.history.read().iter() {
                rx.queue.push(x.clone());
            }
            sched().thread_wake(tid, false);
        }

        rx
    }

    /// The calling thread unsubscribes.
    pub fn unsubscribe(&self) -> bool {
        let tid = sched().current_id();
        self.subscribers.write().remove(&tid).is_some()
    }

    /// Broadcasts a message to all subscribers. Waking the threads.
    pub fn broadcast(&self, value: T) {
        if self.send_history {
            self.history.write().push(value.clone());
        }
        let subs = self.subscribers.read();
        let sched = sched();
        let mut to_remove = Vec::new();
        for (tid, receiver) in subs.iter() {
            if receiver.queue.len() > self.bound {
                receiver.queue.pop();
            }
            receiver.queue.push(value.clone());

            if !sched.thread_exists(tid.clone()) {
                to_remove.push(tid);
            } else {
                sched.thread_wake(tid.clone(), true);
            }
        }

        if !to_remove.is_empty() {
            let mut subs = self.subscribers.write();
            for tid in to_remove {
                subs.remove(tid);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Receiver<T> {
    queue: Arc<SegQueue<T>>,
}

impl<T> Default for Receiver<T> {
    fn default() -> Self {
        Self {
            queue: Arc::new(SegQueue::new()),
        }
    }
}

#[derive(Debug, Error)]
pub enum ReceiveError {
    #[error("timeout")]
    Timeout,
}

impl<T> Receiver<T> {
    /// Try to receive a value.
    ///
    /// This call doesn't block.
    pub fn try_recv(&self) -> Option<T> {
        self.queue.pop()
    }

    /// Blocking receive with a timeout
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, ReceiveError> {
        if let Some(v) = self.try_recv() {
            return Ok(v);
        }

        sched().thread_wait_timeout(timeout);

        if let Some(v) = self.try_recv() {
            Ok(v)
        } else {
            Err(ReceiveError::Timeout)
        }
    }
}
