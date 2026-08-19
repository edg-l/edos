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
    arch::asm,
    fmt,
    mem::offset_of,
    ops::{Deref, DerefMut},
    sync::atomic::AtomicU32,
};
use spin::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::util::per_cpu::PerCpuData;

/// Byte offset of the count within `PerCpuData`, for the GS-relative accessors.
const PREEMPT_COUNT_OFF: usize = offset_of!(PerCpuData, preempt_count);

/// Suppress preemption on this CPU until the returned guard is dropped.
///
/// Nesting is counted, so an inner guard does not re-enable preemption for an
/// outer one.
///
/// The increment is a single GS-relative instruction, and that is load-bearing
/// rather than an optimization: the count belongs to a CPU, not to a thread,
/// and the caller is still preemptible right up to the moment it lands. Reading
/// the GS base into a register and incrementing through it afterwards is two
/// instructions, and a tick landing between them moves the thread to another
/// CPU — the increment then raises the count of the CPU it left, and the guard
/// drop lowers the count of the one it arrived on, wrapping that CPU's count
/// below zero. Both CPUs are wrong from then on, permanently. An instruction
/// cannot be split by an interrupt, and once the count is raised the thread
/// cannot be moved, so the pair stays on one CPU.
#[inline]
pub fn preempt_disable() -> PreemptGuard {
    // SAFETY: kernel GS holds this CPU's `PerCpuData`, and the offset names a
    // field of it. Only this CPU writes the count, so no lock prefix is needed.
    unsafe {
        asm!(
            "add dword ptr gs:[{off}], 1",
            off = const PREEMPT_COUNT_OFF,
            options(nostack),
        );
    }
    PreemptGuard { _private: () }
}

/// This CPU's suppression count.
#[inline]
fn preempt_count() -> u32 {
    let count: u32;
    // SAFETY: as `preempt_disable`, and this one only reads.
    unsafe {
        asm!(
            "mov {count:e}, dword ptr gs:[{off}]",
            count = out(reg) count,
            off = const PREEMPT_COUNT_OFF,
            options(nostack, preserves_flags, readonly),
        );
    }
    count
}

/// Whether this CPU may currently be preempted.
///
/// `Scheduler::maybe_preempt` consults this. A tick that finds preemption
/// suppressed leaves `NEED_RESCHED` set, so the reschedule happens on the next
/// tick instead of being lost.
#[inline]
pub fn preempt_enabled() -> bool {
    preempt_count() == 0
}

/// RAII counterpart of `preempt_disable`.
pub struct PreemptGuard {
    _private: (),
}

impl Drop for PreemptGuard {
    #[inline]
    fn drop(&mut self) {
        // A count that wraps below zero leaves the CPU permanently
        // non-preemptible, and the thread that trips over it is never the one
        // that caused it. Name it here instead.
        debug_assert!(
            preempt_count() > 0,
            "preemption count underflow: a guard was released on a CPU that never took one"
        );
        // SAFETY: as `preempt_disable`; the count is non-zero here, so this
        // thread cannot be moved between reading GS and the decrement either.
        unsafe {
            asm!(
                "sub dword ptr gs:[{off}], 1",
                off = const PREEMPT_COUNT_OFF,
                options(nostack),
            );
        }
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
