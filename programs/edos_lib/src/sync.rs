//! Locks that tell the kernel who holds them.
//!
//! [`std::sync::Mutex`] on this target is a three-state futex word — unlocked,
//! locked, contended — which is the right representation for a lock nobody
//! needs to reason about the holder of, and the wrong one for priority
//! inheritance: a waiter that blocks on it leaves the kernel with an address
//! and no owner, so a holder of middling priority can be kept off the CPU by a
//! thread that wants neither the lock nor anything behind it, and the wait of
//! the most important thread in the process is set by that unrelated thread.
//!
//! [`PiMutex`] answers that by putting the owner **in** the word, so the waiter
//! can name it. See `SYS_FUTEX_WAIT_PI` in the kernel for the discipline the
//! loan follows and what a wrong owner can and cannot do.

use std::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::sys::{SYS_FUTEX_WAIT_PI, SYS_FUTEX_WAKE, SYS_GETTID, syscall0, syscall2, syscall4};

/// Set on the lock word while at least one thread is blocked on it, so an
/// uncontended release needs no syscall to know nobody is waiting.
const WAITERS: u32 = 1 << 31;
/// The owner's thread id occupies everything below [`WAITERS`].
const TID_MASK: u32 = !WAITERS;

/// The calling thread's own id.
///
/// Distinct from `getpid`, which answers for the process and so gives every
/// thread in one the same number.
pub fn gettid() -> u32 {
    (unsafe { syscall0(SYS_GETTID) }) as u32
}

fn futex_wait_pi(word: &AtomicU32, expected: u32, owner_tid: u32) {
    unsafe {
        syscall4(
            SYS_FUTEX_WAIT_PI,
            word as *const AtomicU32 as u64,
            expected as u64,
            u64::MAX, // no timeout
            owner_tid as u64,
        );
    }
}

fn futex_wake(word: &AtomicU32, count: u32) {
    unsafe {
        syscall2(
            SYS_FUTEX_WAKE,
            word as *const AtomicU32 as u64,
            count as u64,
        );
    }
}

/// A mutex whose word names its holder, so a waiter can lend it a priority.
///
/// The word is 0 when free, and otherwise the owner's thread id with
/// [`WAITERS`] set once anyone has blocked. Reading an owner out of it is what
/// lets the wait be a `futex_wait_pi` rather than a `futex_wait`.
///
/// Not a replacement for [`std::sync::Mutex`]: it costs a `gettid` per thread
/// and gives up the uncontended-fast-path tricks std's has. Reach for it where
/// a section is held by threads of different priorities and a slow one would
/// hold up an important one — not by default.
pub struct PiMutex<T> {
    word: AtomicU32,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for PiMutex<T> {}
unsafe impl<T: Send> Sync for PiMutex<T> {}

impl<T> PiMutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            word: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> PiMutexGuard<'_, T> {
        let me = gettid();
        if self
            .word
            .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return PiMutexGuard { lock: self };
        }
        self.lock_contended(me)
    }

    #[cold]
    fn lock_contended(&self, me: u32) -> PiMutexGuard<'_, T> {
        loop {
            let observed = self.word.load(Ordering::Acquire);
            if observed == 0 {
                if self
                    .word
                    .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return PiMutexGuard { lock: self };
                }
                continue;
            }

            // Announce the wait before sleeping on the value that carries the
            // announcement, so a release between the two sees the flag and
            // wakes us rather than returning to an empty word.
            let announced = observed | WAITERS;
            if observed != announced
                && self
                    .word
                    .compare_exchange(observed, announced, Ordering::AcqRel, Ordering::Relaxed)
                    .is_err()
            {
                continue;
            }
            futex_wait_pi(&self.word, announced, announced & TID_MASK);
        }
    }

    /// Whether the lock is held, and by whom. For tests and diagnostics.
    pub fn owner(&self) -> u32 {
        self.word.load(Ordering::Acquire) & TID_MASK
    }
}

pub struct PiMutexGuard<'a, T> {
    lock: &'a PiMutex<T>,
}

impl<T> Deref for PiMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for PiMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for PiMutexGuard<'_, T> {
    fn drop(&mut self) {
        // One store gives up the lock and reads whether anyone was waiting, so
        // the uncontended release is a store and a branch with no syscall.
        if self.lock.word.swap(0, Ordering::Release) & WAITERS != 0 {
            futex_wake(&self.lock.word, 1);
        }
    }
}
