use core::sync::atomic::{AtomicBool, Ordering};

use alloc::{collections::vec_deque::VecDeque, sync::Arc};
use spin::Mutex;

use crate::thread::waitqueue::WaitQueue;

/// A single request with a response slot
pub struct Request<T, R> {
    pub payload: T,
    resp: Arc<ResponseInner<R>>,
}

struct ResponseInner<R> {
    ready: AtomicBool,
    value: Mutex<Option<R>>,
    waitq: WaitQueue,
}

/// Sender’s handle to await a reply
pub struct Response<R> {
    inner: Arc<ResponseInner<R>>,
}

impl<R> Response<R> {
    pub fn wait(self) -> R {
        while !self.inner.ready.load(Ordering::Acquire) {
            self.inner.waitq.wait();
        }
        self.inner.value.lock().take().unwrap()
    }

    pub fn try_get(&self) -> Option<R> {
        if self.inner.ready.load(Ordering::Acquire) {
            self.inner.value.lock().take()
        } else {
            None
        }
    }
}

/// Receiver side mailbox
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
            payload,
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
            self.not_empty.wait();
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
}
