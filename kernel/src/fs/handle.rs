use core::{
    fmt::Debug,
    sync::atomic::{AtomicU8, Ordering},
};

use alloc::{sync::Arc, vec::Vec};

use crate::fs::PollState;
use crate::thread::poll::PollWaiter;

pub type PollKey = u64;

/// One descriptor's readiness within a caller's poll set.
#[derive(Debug)]
struct PollSlot {
    interests: PollState,
    state: AtomicU8,
}

impl PollSlot {
    const EMPTY: Self = Self {
        interests: PollState::none(),
        state: AtomicU8::new(0),
    };
}

/// Descriptor counts at or below this keep their slots inside the `PollSet`
/// allocation. A slot is six bytes, so the array costs almost nothing to
/// initialise, and it takes the whole set down to one allocation for the
/// counts poll is usually called with.
const INLINE_SLOTS: usize = 8;

#[derive(Debug)]
enum Slots {
    Inline { slots: [PollSlot; INLINE_SLOTS] },
    Heap(Vec<PollSlot>),
}

impl core::ops::Index<usize> for Slots {
    type Output = PollSlot;

    fn index(&self, index: usize) -> &PollSlot {
        match self {
            Self::Inline { slots } => &slots[index],
            Self::Heap(slots) => &slots[index],
        }
    }
}

/// Every descriptor of one `poll` call, and the waiter they all wake.
///
/// One allocation serves the whole call: a device holds a [`PollRef`], which
/// is a refcount on this plus an index, so registering a descriptor costs no
/// allocation at all. Per-descriptor entries cost two allocations each, which
/// measured at 82 ns of a 158 ns descriptor.
#[derive(Debug)]
pub struct PollSet {
    waiter: PollWaiter,
    slots: Slots,
}

impl PollSet {
    /// A set with one slot per descriptor, in the caller's order.
    pub fn new(waiter: PollWaiter, interests: impl ExactSizeIterator<Item = PollState>) -> Self {
        let slots = if interests.len() <= INLINE_SLOTS {
            let mut slots = [const { PollSlot::EMPTY }; INLINE_SLOTS];
            for (slot, interests) in slots.iter_mut().zip(interests) {
                slot.interests = interests;
            }
            Slots::Inline { slots }
        } else {
            Slots::Heap(
                interests
                    .map(|interests| PollSlot {
                        interests,
                        state: AtomicU8::new(PollState::none().to_bits()),
                    })
                    .collect(),
            )
        };
        Self { waiter, slots }
    }

    pub fn state(&self, slot: usize) -> PollState {
        PollState::from_bits(self.slots[slot].state.load(Ordering::Acquire))
    }

    /// Clear the pending flag, returning whether a notification was pending.
    pub fn arm(&self) -> bool {
        self.waiter.arm()
    }
}

/// A device's handle on one descriptor of one poll call.
///
/// Cloning is a refcount bump, which is what lets a poller list hold one per
/// registered descriptor without allocating.
#[derive(Debug, Clone)]
pub struct PollRef {
    set: Arc<PollSet>,
    slot: usize,
}

impl PollRef {
    pub fn new(set: &Arc<PollSet>, slot: usize) -> Self {
        Self {
            set: Arc::clone(set),
            slot,
        }
    }

    pub fn interests(&self) -> PollState {
        self.set.slots[self.slot].interests
    }

    pub fn update(&self, state: PollState) {
        let slot = &self.set.slots[self.slot];
        slot.state.store(state.to_bits(), Ordering::Release);
        if state.matches(slot.interests) {
            self.set.waiter.notify();
        }
    }
}

#[derive(Debug, Clone)]
pub struct PollRegistration {
    pub initial: PollState,
    /// `None` when the device kept no registration, so the reported readiness
    /// can never change and there is nothing to unregister.
    pub key: Option<PollKey>,
}

pub trait Pollable: Send + Sync + Debug {
    fn register(&self, entry: PollRef) -> PollRegistration;

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
    fn register(&self, entry: PollRef) -> PollRegistration {
        entry.update(self.0);
        PollRegistration {
            initial: self.0,
            key: None,
        }
    }
}
