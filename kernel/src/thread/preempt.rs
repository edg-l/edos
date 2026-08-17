//! Preemption control and the spin locks built on it.
//!
//! A spin lock is only bounded if its holder keeps running: every other CPU
//! busy-waits behind it. Preemption is involuntary, so a bare `spin::Mutex`
//! guard can be held by a descheduled thread, and every waiter then burns its
//! CPU until that thread is scheduled again.
//!
//! Disabling interrupts fixes that, but it is the wrong tool for a lock held
//! across real work — `memory_manager` walks page tables, `vmas` walks the VMA
//! tree — because the whole section is charged to interrupt latency. Suppressing
//! *preemption* is enough: the holder keeps its CPU until it releases, while
//! interrupts continue to be serviced normally.
//!
//! Use `IrqSpinlock` for state an interrupt handler can reach, and the locks
//! here for everything else.

use core::{
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
};
use spin::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::util::per_cpu::get_percpu_data;

/// Suppress preemption on this CPU until the returned guard is dropped.
///
/// Nesting is counted, so an inner guard does not re-enable preemption for an
/// outer one.
#[inline]
pub fn preempt_disable() -> PreemptGuard {
    get_percpu_data()
        .preempt_count
        .fetch_add(1, Ordering::Acquire);
    PreemptGuard { _private: () }
}

/// Whether this CPU may currently be preempted.
///
/// `Scheduler::maybe_preempt` consults this. A tick that finds preemption
/// suppressed leaves `NEED_RESCHED` set, so the reschedule happens on the next
/// tick instead of being lost.
#[inline]
pub fn preempt_enabled() -> bool {
    get_percpu_data().preempt_count.load(Ordering::Acquire) == 0
}

/// RAII counterpart of `preempt_disable`.
pub struct PreemptGuard {
    _private: (),
}

impl Drop for PreemptGuard {
    #[inline]
    fn drop(&mut self) {
        get_percpu_data()
            .preempt_count
            .fetch_sub(1, Ordering::Release);
    }
}

/// Assert that the calling context is allowed to block or switch away.
///
/// Parking with preemption suppressed means parking while a spin lock is held,
/// which wedges every CPU waiting on that lock.
#[inline]
pub fn debug_assert_preemptible(what: &str) {
    debug_assert!(
        preempt_enabled(),
        "{what} with preemption disabled (spin lock held across a blocking operation)"
    );
}

/// A mutex whose guard suppresses preemption for its lifetime.
pub struct PreemptSpinlock<T> {
    inner: Mutex<T>,
}

impl<T> fmt::Debug for PreemptSpinlock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreemptSpinlock { <contents omitted> }")
    }
}

impl<T> PreemptSpinlock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
        }
    }

    pub fn lock(&self) -> PreemptMutexGuard<'_, T> {
        let preempt = preempt_disable();
        PreemptMutexGuard {
            guard: Some(self.inner.lock()),
            _preempt: preempt,
        }
    }

    /// Take the lock if it is free, rather than waiting for it.
    ///
    /// For a caller whose work is safe to skip and which must not be charged
    /// the holder's critical section — a display cursor move on the input
    /// path, where the same lock is also held across a full-screen blit and
    /// the position is superseded by the next report either way.
    pub fn try_lock(&self) -> Option<PreemptMutexGuard<'_, T>> {
        let preempt = preempt_disable();
        match self.inner.try_lock() {
            Some(guard) => Some(PreemptMutexGuard {
                guard: Some(guard),
                _preempt: preempt,
            }),
            // Re-enable preemption rather than holding it over a failed try.
            None => {
                drop(preempt);
                None
            }
        }
    }
}

pub struct PreemptMutexGuard<'a, T> {
    // Declared before `_preempt` so the lock is released before preemption is
    // re-enabled.
    guard: Option<MutexGuard<'a, T>>,
    _preempt: PreemptGuard,
}

impl<T> Deref for PreemptMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<T> DerefMut for PreemptMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap()
    }
}

/// A reader-writer lock whose guards suppress preemption for their lifetime.
pub struct PreemptRwLock<T> {
    inner: RwLock<T>,
}

impl<T> fmt::Debug for PreemptRwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreemptRwLock { <contents omitted> }")
    }
}

impl<T> PreemptRwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: RwLock::new(value),
        }
    }

    pub fn read(&self) -> PreemptReadGuard<'_, T> {
        let preempt = preempt_disable();
        PreemptReadGuard {
            guard: Some(self.inner.read()),
            _preempt: preempt,
        }
    }

    pub fn write(&self) -> PreemptWriteGuard<'_, T> {
        let preempt = preempt_disable();
        PreemptWriteGuard {
            guard: Some(self.inner.write()),
            _preempt: preempt,
        }
    }
}

pub struct PreemptReadGuard<'a, T> {
    guard: Option<RwLockReadGuard<'a, T>>,
    _preempt: PreemptGuard,
}

impl<T> Deref for PreemptReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

pub struct PreemptWriteGuard<'a, T> {
    guard: Option<RwLockWriteGuard<'a, T>>,
    _preempt: PreemptGuard,
}

impl<T> Deref for PreemptWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<T> DerefMut for PreemptWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap()
    }
}

/// Per-CPU preemption suppression count.
pub type PreemptCount = AtomicU32;
