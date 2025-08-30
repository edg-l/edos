//! Signas to the scheduler

use crate::thread::scheduler::sched;

#[derive(Debug)]
pub enum Signal {
    // Exit signal
    Exit(i32),
}

/// Sends a signal from the current thread
pub fn send_signal(signal: Signal) {
    let sched = sched();
    let id = sched.current_id();
    sched.signal_queue.push((id, signal));
}
