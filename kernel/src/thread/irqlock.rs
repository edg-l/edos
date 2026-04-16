use core::{
    fmt,
    ops::{Deref, DerefMut},
};
use spin::{Mutex, MutexGuard};
use x86_64::instructions::interrupts;

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

    /// Disable local IRQs, take the lock, remember prior IF.
    pub fn lock(&self) -> IrqLockGuard<'_, T> {
        let prev_if = interrupts::are_enabled();
        if prev_if {
            interrupts::disable();
        }
        // Take the lock with IRQs off to avoid deadlock with IRQ handlers.
        let guard = self.inner.lock();
        IrqLockGuard {
            guard: Some(guard),
            prev_if,
        }
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
