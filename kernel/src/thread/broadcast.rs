use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use crossbeam_queue::SegQueue;
use spin::Mutex;
use thiserror::Error;
use x86_64::instructions::interrupts::without_interrupts;

use crate::thread::scheduler::sched;

#[derive(Debug)]
pub struct Broadcast<T: Clone> {
    subscribers: BTreeMap<u64, Receiver<T>>,
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

            let rx = (*self.subscribers.entry(tid).or_default()).clone();

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
                let tid = *tid;
                if receiver.queue.len() > self.bound {
                    receiver.queue.pop();
                }
                receiver.queue.push(value.clone());

                if !sched.thread_exists(tid) {
                    to_remove.push(tid);
                } else if receiver.is_waiting.load(Ordering::Acquire) {
                    sched.thread_wake(tid, true);
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
    is_waiting: Arc<AtomicBool>,
}

impl<T> Default for Receiver<T> {
    fn default() -> Self {
        Self {
            queue: Arc::new(SegQueue::new()),
            is_waiting: Arc::new(AtomicBool::new(false)),
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

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns immediately if there is content, otherwise
    /// waits until timeout waiting for content, returning
    /// true if there is content to receive.
    pub fn poll(&self, timeout: Duration) -> bool {
        if !self.is_empty() {
            return true;
        }

        self.is_waiting.store(true, Ordering::Release);

        sched().thread_wait_timeout(timeout);

        self.is_waiting.store(false, Ordering::Release);

        !self.is_empty()
    }

    /// Blocking receive with a timeout
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, ReceiveError> {
        if let Some(v) = self.try_recv() {
            return Ok(v);
        }

        self.is_waiting.store(true, Ordering::Release);

        sched().thread_wait_timeout(timeout);

        self.is_waiting.store(false, Ordering::Release);

        if let Some(v) = self.try_recv() {
            Ok(v)
        } else {
            Err(ReceiveError::Timeout)
        }
    }
}
