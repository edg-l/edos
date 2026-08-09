use core::sync::atomic::{AtomicBool, Ordering};

use alloc::{collections::vec_deque::VecDeque, sync::Arc};

use crate::debug::lock_order::{RANK_MAILBOX_QUEUE, RANK_MAILBOX_RESPONSE};
use crate::ranked_lock;
use crate::thread::{mutex::BlockingMutex, waitqueue::WaitQueue};

/*

Not IRQ safe.

SMP Safe if:

Only one thread ever calls reply() on a given Request.
Only one thread ever calls Response::wait() or try_get() (so only one consumer).
You do not reuse a Request after reply().

Then:
There’s no race where two writers fight over value.
The ready.store(Release) + wait() loop with Acquire ensures the consumer sees the written value.
The waitq wake after the Release store prevents lost-wake.
*/

/// A single request with a response slot
#[derive(Debug)]
pub struct Request<T, R> {
    pub payload: Option<T>,
    resp: Arc<ResponseInner<R>>,
}

#[derive(Debug)]
struct ResponseInner<R> {
    ready: AtomicBool,
    value: BlockingMutex<Option<R>>,
    waitq: WaitQueue,
}

/// Sender’s handle to await a reply
#[derive(Debug)]
pub struct Response<R> {
    inner: Arc<ResponseInner<R>>,
}

impl<R> Response<R> {
    pub fn wait(self) -> R {
        while !self.inner.ready.load(Ordering::Acquire) {
            let _ = self
                .inner
                .waitq
                .wait_until(|| self.inner.ready.load(Ordering::Acquire));
        }
        ranked_lock!(RANK_MAILBOX_RESPONSE, "Response::wait", self.inner.value)
            .take()
            .unwrap()
    }
}

/// Receiver side mailbox
#[derive(Debug)]
pub struct Mailbox<T, R> {
    queue: BlockingMutex<VecDeque<Request<T, R>>>,
    not_empty: WaitQueue,
}

impl<T, R> Mailbox<T, R> {
    pub const fn new() -> Self {
        Self {
            queue: BlockingMutex::new(VecDeque::new()),
            not_empty: WaitQueue::new(),
        }
    }

    /// Create a mailbox with pre-allocated queue capacity to avoid
    /// heap allocation under the queue lock during `send()`.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            queue: BlockingMutex::new(VecDeque::with_capacity(cap)),
            not_empty: WaitQueue::new(),
        }
    }

    pub fn send(&self, payload: T) -> Response<R> {
        let inner = Arc::new(ResponseInner {
            ready: AtomicBool::new(false),
            value: BlockingMutex::new(None),
            waitq: WaitQueue::new(),
        });
        let resp = Response {
            inner: inner.clone(),
        };
        let req = Request {
            payload: Some(payload),
            resp: inner,
        };

        {
            let mut q = ranked_lock!(RANK_MAILBOX_QUEUE, "Mailbox::send", self.queue);
            q.push_back(req);
        }
        self.not_empty.wake_one();
        resp
    }

    pub fn recv(&self) -> Request<T, R> {
        loop {
            {
                let mut q = ranked_lock!(RANK_MAILBOX_QUEUE, "Mailbox::recv", self.queue);
                if let Some(req) = q.pop_front() {
                    return req;
                }
            }
            // Park until a sender wakes us. The closure re-checks under lock
            // to avoid lost wakeups.
            let _ = self.not_empty.wait_until(|| {
                !ranked_lock!(RANK_MAILBOX_QUEUE, "Mailbox::recv_wait", self.queue).is_empty()
            });
        }
    }

    /// Check if the mailbox has pending requests without consuming them.
    /// Uses try_lock so it's safe to call with interrupts disabled.
    pub fn is_empty(&self) -> bool {
        match self.queue.try_lock() {
            Some(q) => q.is_empty(),
            None => false, // lock contended, assume not empty to avoid missing a request
        }
    }

    /// Non-blocking receive: returns `Some(request)` if one is available, `None` otherwise.
    pub fn try_recv(&self) -> Option<Request<T, R>> {
        ranked_lock!(RANK_MAILBOX_QUEUE, "Mailbox::try_recv", self.queue).pop_front()
    }

    #[allow(dead_code)]
    pub fn reply(req: Request<T, R>, val: R) {
        {
            let mut slot = ranked_lock!(RANK_MAILBOX_RESPONSE, "Mailbox::reply", req.resp.value);
            **slot = Some(val);
        }
        // Fence ensures the value write is globally visible before ready flag.
        // The BlockingMutex release and ready store are on different atomics,
        // so without this fence they are not ordered with respect to each other.
        core::sync::atomic::fence(Ordering::Release);
        req.resp.ready.store(true, Ordering::Relaxed);
        req.resp.waitq.wake_one();
    }

    #[expect(unused)]
    pub fn forward<U>(&self, req: Request<U, R>, payload: T) {
        let new_req = Request {
            payload: Some(payload),
            resp: req.resp, // keep same response channel
        };

        {
            let mut q = ranked_lock!(RANK_MAILBOX_QUEUE, "Mailbox::forward", self.queue);
            q.push_back(new_req);
        }
        self.not_empty.wake_one();
    }
}

impl<T, R> Request<T, R> {
    pub fn reply(&self, val: R) {
        {
            let mut slot = ranked_lock!(RANK_MAILBOX_RESPONSE, "Request::reply", self.resp.value);
            **slot = Some(val);
        }
        core::sync::atomic::fence(Ordering::Release);
        self.resp.ready.store(true, Ordering::Relaxed);
        self.resp.waitq.wake_one();
    }
}
