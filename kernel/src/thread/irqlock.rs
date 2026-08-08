use core::{
    fmt,
    ops::{Deref, DerefMut},
};
use spin::{Mutex, MutexGuard};
use x86_64::instructions::interrupts;

use crate::debug::lock_order::RankedGuard;

pub struct IrqSpinlock<T> {
    inner: Mutex<T>,
}

impl<T> fmt::Debug for IrqSpinlock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IrqSpinlock { <contents omitted> }")
    }
}

impl<T> IrqSpinlock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
        }
    }

    /// Take the lock, then disable local IRQs for the guard's lifetime.
    ///
    /// IRQs are off only while the lock is *held*, which is what keeps an IRQ
    /// handler from deadlocking against the holder. Waiting happens with the
    /// caller's original IF, because a CPU that spins with interrupts disabled
    /// answers no IPIs for the whole wait: a TLB shootdown initiator on another
    /// CPU then spins until its timeout and gives up on a flush that never
    /// happened. Re-entering the same lock from an IRQ taken while waiting is
    /// harmless — the waiter does not hold it yet.
    pub fn lock(&self) -> IrqLockGuard<'_, T> {
        let prev_if = interrupts::are_enabled();
        loop {
            if prev_if {
                interrupts::disable();
            }
            if let Some(guard) = self.inner.try_lock() {
                return IrqLockGuard {
                    guard: Some(guard),
                    prev_if,
                };
            }
            if prev_if {
                interrupts::enable();
            }
            // Read-only spin off the contended cache line until it looks free,
            // rather than hammering the CAS.
            while self.inner.is_locked() {
                core::hint::spin_loop();
            }
        }
    }

    /// Acquire the lock and push `rank` onto the per-thread lock-rank stack.
    /// Returns a `RankedGuard` that pops the rank on drop AFTER releasing the
    /// inner `IrqLockGuard`. Zero-cost in release builds.
    #[allow(dead_code)]
    pub fn lock_ranked(&self, rank: u16, site: &'static str) -> RankedGuard<IrqLockGuard<'_, T>> {
        crate::debug::lock_order::enter(rank, site);
        let inner = self.lock();
        RankedGuard::new(inner, rank, site)
    }

    /// Try to lock. If successful, IRQs are disabled until drop.
    pub fn try_lock(&self) -> Option<IrqLockGuard<'_, T>> {
        let prev_if = interrupts::are_enabled();
        if prev_if {
            interrupts::disable();
        }
        match self.inner.try_lock() {
            Some(g) => Some(IrqLockGuard {
                guard: Some(g),
                prev_if,
            }),
            None => {
                // Restore IF if we didn’t get the lock.
                if prev_if {
                    interrupts::enable();
                }
                None
            }
        }
    }
}

pub struct IrqLockGuard<'a, T> {
    guard: Option<MutexGuard<'a, T>>,
    prev_if: bool,
}

impl<'a, T> Deref for IrqLockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // Safe: guard is Some while this exists.
        self.guard.as_ref().unwrap()
    }
}

impl<'a, T> DerefMut for IrqLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.guard.as_mut().unwrap()
    }
}

impl<'a, T> Drop for IrqLockGuard<'a, T> {
    fn drop(&mut self) {
        // 1) Release the spin::MutexGuard before re-enabling IRQs.
        let _ = self.guard.take();
        // 2) Restore IF only if it was enabled on entry.
        if self.prev_if {
            interrupts::enable();
        }
    }
}
