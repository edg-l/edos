use core::time::Duration;

use alloc::collections::vec_deque::VecDeque;

use crate::thread::{scheduler::sched, ThreadId};

pub struct Mailbox<T> {
    queue: VecDeque<T>,
    waiting_thread: Option<ThreadId>,
}

pub enum ReceiveResult<T> {
    Message(T),
    Timeout,
    NoMatch,
}

impl<T> Mailbox<T> {
    pub fn send(&mut self, message: T) {
        self.queue.push_back(message);

        // Wake up waiting thread if any
        if let Some(thread_id) = self.waiting_thread.take() {
            sched().thread_wake(thread_id);
        }
    }

    pub fn try_receive<F>(&mut self, predicate: F) -> Option<T>
    where
        F: Fn(&T) -> bool
    {
        // Find first matching message
        if let Some(pos) = self.queue.iter().position(&predicate) {
            self.queue.remove(pos)
        } else {
            None
        }
    }

    pub fn receive_timeout<F>(&mut self, predicate: F, timeout: Duration) -> ReceiveResult<T>
    where
        F: Fn(&T) -> bool
    {
        // Try immediate receive
        if let Some(msg) = self.try_receive(&predicate) {
            return ReceiveResult::Message(msg);
        }

        // Mark thread as waiting and yield
        let sched = sched();
        let thread_id = sched.current_id();
        self.waiting_thread = Some(thread_id);
        sched.thread_wait(thread_id, timeout);

        // Try again after wakeup
        if let Some(msg) = self.try_receive(predicate) {
            ReceiveResult::Message(msg)
        } else {
            ReceiveResult::Timeout
        }
    }
}
