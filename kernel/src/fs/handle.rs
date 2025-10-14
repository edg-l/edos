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
