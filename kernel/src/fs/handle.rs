use core::{
    fmt::Debug,
    sync::atomic::{AtomicU8, Ordering},
};

use alloc::sync::Arc;

use crate::fs::PollState;
use crate::thread::poll::PollWaiter;

pub type PollKey = u64;

#[derive(Debug)]
pub struct PollEntry {
    waiter: Arc<PollWaiter>,
    interests: PollState,
    state: AtomicU8,
}

impl PollEntry {
    pub fn new(waiter: Arc<PollWaiter>, interests: PollState) -> Self {
        Self {
            waiter,
            interests,
            state: AtomicU8::new(PollState::none().to_bits()),
        }
    }

    pub fn interests(&self) -> PollState {
        self.interests
    }

    pub fn state(&self) -> PollState {
        PollState::from_bits(self.state.load(Ordering::Acquire))
    }

    pub fn update(&self, state: PollState) {
        self.state.store(state.to_bits(), Ordering::Release);
        if state.matches(self.interests) {
            self.waiter.notify();
        }
    }
}

#[derive(Debug, Clone)]
pub struct PollRegistration {
    pub initial: PollState,
    pub key: Option<PollKey>,
}

pub trait Pollable: Send + Sync + Debug {
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration;

    fn unregister(&self, _key: PollKey) {}
}

/// A `Pollable` for something that never blocks.
///
/// Its readiness cannot change, so the registration answers from the fixed
/// state and keeps no entry: there is no event a waiter could ever be waiting
/// for. Regular files, the console streams and the data-sink devices all report
/// this way, which is what stops a caller seeing `POLLERR | POLLNVAL` for a
/// descriptor that works perfectly well.
#[derive(Debug)]
pub struct StaticPoll(PollState);

impl StaticPoll {
    pub const fn new(state: PollState) -> Self {
        Self(state)
    }

    /// Ready in both directions, which is what POSIX requires a regular file to
    /// report.
    pub const fn ready() -> Self {
        Self(PollState {
            readable: true,
            writable: true,
            error: false,
            hangup: false,
            invalid: false,
        })
    }
}

impl Pollable for StaticPoll {
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration {
        entry.update(self.0);
        PollRegistration {
            initial: self.0,
            key: None,
        }
    }
}
