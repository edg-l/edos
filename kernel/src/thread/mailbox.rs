use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use alloc::sync::Arc;
use crossbeam_queue::SegQueue;
use x86_64::instructions::interrupts::without_interrupts;

use crate::{
    serial_println,
    thread::{ThreadId, scheduler::sched},
};

// TODO: maybe make a sender struct so others dont have access to the queue
// Encapsulate better

#[derive(Debug)]
/// A mailmox for sending requests with the type T and getting responses with the type R.
pub struct Mailbox<T, R> {
    pub queue: SegQueue<Request<T, R>>,
    pub owner: ThreadId,
}

#[derive(Debug)]
/// The message info.
pub struct Request<T, R> {
    /// The sender thread
    pub sender: ThreadId,
    /// The response given when the message was sent.
    ///
    /// Saved so we can place the value in the response when the message is processed.
    pub response: Arc<Response<R>>,
    /// The message
    pub request: T,
}

impl<T, R> Mailbox<T, R> {
    pub fn new(owner: ThreadId) -> Self {
        Self {
            owner,
            queue: SegQueue::new(),
        }
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
                sender,
                request,
                response: response.clone(),
            });

            sched().thread_wake(self.owner);
            response
        })
    }
}

#[derive(Debug)]
pub struct Response<R> {
    /// Whether the response is fulfilled.
    fulfilled: AtomicBool,
    taken: AtomicBool,
    // Thread to wake on response.
    thread: ThreadId,
    value: UnsafeCell<MaybeUninit<R>>,
}

unsafe impl<R: Send> Send for Response<R> {}
unsafe impl<R: Send> Sync for Response<R> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResponseResult<R> {
    Value(R),
    Timeout,
    AlreadyClaimed,
}

impl<R> Response<R> {
    pub fn answer(&self, value: R) {
        without_interrupts(|| {
            unsafe { self.value.get().write(MaybeUninit::new(value)) };
            self.fulfilled.store(true, Ordering::Release);
            let sched = sched();
            sched.thread_wake(self.thread);
        })
    }

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
    pub fn receive_timeout(&self, timeout: Duration) -> ResponseResult<R> {
        // Try immediate receive
        if let Some(value) = self.try_receive() {
            return ResponseResult::Value(value);
        }

        // Mark thread as waiting and yield
        let sched = sched();
        let thread_id = sched.current_id();
        // Mark ourselves as waiting.
        sched.thread_wait(thread_id, timeout);
        // Yield to scheduler.
        sched.thread_yield();

        // Try again after wakeup
        if let Some(msg) = self.try_receive() {
            ResponseResult::Value(msg)
        } else {
            ResponseResult::Timeout
        }
    }
}
