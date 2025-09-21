use core::{fmt::Debug, time::Duration};

use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use crate::fs::PollState;

pub trait Pollable: Send + Sync + Debug {
    fn poll(&self, timeout: Duration) -> PollState;
}
