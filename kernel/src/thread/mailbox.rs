use core::sync::atomic::{AtomicBool, Ordering};

use alloc::{collections::vec_deque::VecDeque, sync::Arc};
use spin::Mutex;

use crate::{log, thread::waitqueue::WaitQueue};

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
    value: Mutex<Option<R>>,
    waitq: WaitQueue,
}

/// Sender’s handle to await a reply
#[derive(Debug)]
pub struct Response<R> {
    inner: Arc<ResponseInner<R>>,
}

impl<R> Response<R> {
    pub fn wait(self) -> R {
        log!("Waiting for response");
        while !self.inner.ready.load(Ordering::Acquire) {
            self.inner
                .waitq
                .wait_until(|| self.inner.ready.load(Ordering::Acquire));
        }
           log!("Got response");
        self.inner.value.lock().take().unwrap()
    }
}

/// Receiver side mailbox
#[derive(Debug)]
pub struct Mailbox<T, R> {
    queue: Mutex<VecDeque<Request<T, R>>>,
    not_empty: WaitQueue,
}

impl<T, R> Mailbox<T, R> {
    pub const fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            not_empty: WaitQueue::new(),
        }
    }

    pub fn send(&self, payload: T) -> Response<R> {
        let inner = Arc::new(ResponseInner {
            ready: AtomicBool::new(false),
            value: Mutex::new(None),
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
            let mut q = self.queue.lock();
            q.push_back(req);
        }
        self.not_empty.wake_one();
        resp
    }

    pub fn recv(&self) -> Request<T, R> {
        loop {
            if let Some(req) = self.queue.lock().pop_front() {
                return req;
            }
            self.not_empty.wait_until(|| !self.queue.lock().is_empty());
        }
    }

    pub fn reply(req: Request<T, R>, val: R) {
        {
            let mut slot = req.resp.value.lock();
            *slot = Some(val);
        }
        req.resp.ready.store(true, Ordering::Release);
        req.resp.waitq.wake_one();
    }

    pub fn forward<U>(&self, req: Request<U, R>, payload: T) {
        let new_req = Request {
            payload: Some(payload),
            resp: req.resp, // keep same response channel
        };

        {
            let mut q = self.queue.lock();
            q.push_back(new_req);
        }
        self.not_empty.wake_one();
    }
}

impl<T, R> Request<T, R> {
    pub fn reply(&self, val: R) {
        {
            let mut slot = self.resp.value.lock();
            *slot = Some(val);
        }
        self.resp.ready.store(true, Ordering::Release);
        self.resp.waitq.wake_one();
    }
}
