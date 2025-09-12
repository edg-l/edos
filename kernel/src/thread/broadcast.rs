use core::time::Duration;

use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use crossbeam_queue::SegQueue;
use spin::Mutex;
use thiserror::Error;
use x86_64::instructions::interrupts::without_interrupts;

use crate::thread::{ThreadId, scheduler::sched};

#[derive(Debug)]
pub struct Broadcast<T: Clone> {
    subscribers: BTreeMap<ThreadId, Receiver<T>>,
    history: Vec<T>,
    send_history: bool,
    bound: usize,
}

pub type LockedBroadcast<T> = Mutex<Broadcast<T>>;

pub const fn new_broadcast<T: Clone>(bound: usize, send_history: bool) -> Mutex<Broadcast<T>> {
    Mutex::new(Broadcast::new(bound, send_history))
}

impl<T: Clone> Broadcast<T> {
    /// If send_history is true when a new subscriber will get all history sent.
    pub const fn new(bound: usize, send_history: bool) -> Self {
        Self {
            subscribers: BTreeMap::new(),
            history: Vec::new(),
            send_history,
            bound,
        }
    }

    /// The calling thread subscribes.
    pub fn subscribe_or_get(&mut self) -> Receiver<T> {
        without_interrupts(|| {
            let tid = sched().current_id();
            if let Some(r) = self.subscribers.get(&tid) {
                return r.clone();
            }

            let rx = (*self.subscribers.entry(tid.clone()).or_default()).clone();

            if self.send_history {
                for x in self.history.iter() {
                    rx.queue.push(x.clone());
                }
                sched().thread_wake(tid, false);
            }

            rx
        })
    }

    /// The calling thread unsubscribes.
    pub fn unsubscribe(&mut self) -> bool {
        let tid = sched().current_id();
        self.subscribers.remove(&tid).is_some()
    }

    /// Broadcasts a message to all subscribers. Waking the threads.
    pub fn broadcast(&mut self, value: T) {
        without_interrupts(|| {
            if self.send_history {
                self.history.push(value.clone());
            }

            let sched = sched();
            let mut to_remove = Vec::new();
            for (tid, receiver) in self.subscribers.iter() {
                if receiver.queue.len() > self.bound {
                    receiver.queue.pop();
                }
                receiver.queue.push(value.clone());

                if !sched.thread_exists(tid.clone()) {
                    to_remove.push(tid.clone());
                } else {
                    sched.thread_wake(tid.clone(), true);
                }
            }

            if !to_remove.is_empty() {
                for tid in to_remove {
                    self.subscribers.remove(&tid);
                }
            }
        })
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
