use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use alloc::sync::Arc;
use crossbeam_queue::SegQueue;
use thiserror::Error;
use x86_64::instructions::interrupts::without_interrupts;

use crate::thread::scheduler::sched;

// TODO: maybe make a sender struct so others dont have access to the queue
// Encapsulate better

#[derive(Debug)]
/// A mailbox for sending requests with the type T and getting responses with the type R.
pub struct Mailbox<T, R> {
    queue: Arc<SegQueue<Request<T, R>>>,
    owner: u64,
    is_waiting: Arc<AtomicBool>,
}

impl<T, R> Clone for Mailbox<T, R> {
    fn clone(&self) -> Self {
        let queue = self.queue.clone();
        let tid = self.owner;

        Self {
            queue,
            owner: tid,
            is_waiting: self.is_waiting.clone(),
        }
    }
}

#[derive(Debug)]
/// The message info.
pub struct Request<T, R> {
    /// The response given when the message was sent.
    ///
    /// Saved so we can place the value in the response when the message is processed.
    pub response: Arc<Response<R>>,
    /// The message.
    pub message: T,
}

impl<T, R> Mailbox<T, R> {
    pub fn new(owner: u64) -> Self {
        Self {
            owner,
            queue: Arc::new(SegQueue::new()),
            is_waiting: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn wait(&self) {
        without_interrupts(|| {
            if self.queue.is_empty() {
                self.is_waiting.store(true, Ordering::Release);

                if !self.queue.is_empty() {
                    self.is_waiting.store(false, Ordering::Release);
                    return;
                }
                sched().thread_park();
                self.is_waiting.store(false, Ordering::Release);
            }
        })
    }

    /// Send a request, returning a Response that can be waited for values.
    pub fn send(&self, request: T) -> Arc<Response<R>> {
        without_interrupts(|| {
            let sender = sched().current_id();
            let response = Arc::new(Response {
                fulfilled: AtomicBool::new(false),
                taken: AtomicBool::new(false),
                value: UnsafeCell::new(MaybeUninit::uninit()),
                thread: sender,
            });

            self.queue.push(Request {
                message: request,
                response: response.clone(),
            });

            if self.is_waiting.swap(false, Ordering::AcqRel) {
                sched().thread_wake(self.owner, true);
            }

            response
        })
    }

    pub fn forward(&self, request: T, response: Arc<Response<R>>) -> Arc<Response<R>> {
        self.queue.push(Request {
            message: request,
            response: response.clone(),
        });

        sched().thread_wake(self.owner, true);
        response
    }

    pub fn pop_request(&self) -> Option<Request<T, R>> {
        self.queue.pop()
    }
}

#[derive(Debug)]
pub struct Response<R> {
    /// Whether the response is fulfilled.
    fulfilled: AtomicBool,
    taken: AtomicBool,
    // Thread to wake on response.
    thread: u64,
    value: UnsafeCell<MaybeUninit<R>>,
}

unsafe impl<R: Send> Send for Response<R> {}
unsafe impl<R: Send> Sync for Response<R> {}

#[derive(Debug, Error)]
pub enum ReceiveError {
    #[error("timeout")]
    Timeout,
    #[error("this response was already received")]
    AlreadyReceived,
}

impl<R> Response<R> {
    // If this method is called after having received a value (Some), it will return None forever.
    pub fn try_receive(&self) -> Option<R> {
        if !self.fulfilled.load(Ordering::Acquire) {
            return None;
        }

        if self.taken.swap(true, Ordering::AcqRel) {
            return None;
        }

        let value = self.value.get();
        Some(unsafe { (*value).assume_init_read() })
    }

    /// Try to receive the result within the given timeout.
    pub fn receive_timeout(&self, timeout: Duration) -> Result<R, ReceiveError> {
        // Try immediate receive
        if let Some(value) = self.try_receive() {
            return Ok(value);
        }

        if self.taken.load(Ordering::Acquire) {
            return Err(ReceiveError::AlreadyReceived);
        }

        // Wait with timeout
        if !timeout.is_zero() {
            sched().thread_wait_timeout(timeout);
        }

        // Try again after wakeup
        if let Some(msg) = self.try_receive() {
            Ok(msg)
        } else {
            Err(ReceiveError::Timeout)
        }
    }

    pub fn send(&self, value: R) {
        without_interrupts(|| {
            unsafe { self.value.get().write(MaybeUninit::new(value)) };
            self.fulfilled.store(true, Ordering::Release);
            sched().thread_wake(self.thread, true);
        })
    }
}
