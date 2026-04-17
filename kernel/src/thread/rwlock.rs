use core::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicI32, Ordering},
};

use crate::{debug::lock_order::RankedGuard, thread::waitqueue::WaitQueue};

/// A reader-writer lock that blocks waiting threads via the scheduler.
///
/// Multiple readers can hold the lock concurrently. A writer has exclusive
/// access. Waiting threads park (yield to the scheduler) rather than spin.
///
/// State encoding (AtomicI32):
///   0        = unlocked
///   positive = number of active readers
///   -1       = writer held
pub struct RwLock<T> {
    state: AtomicI32,
    waiters: WaitQueue,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for RwLock<T> {}
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicI32::new(0),
            waiters: WaitQueue::new(),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquire a shared read lock. Blocks if a writer is active.
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        loop {
            let s = self.state.load(Ordering::Acquire);
            if s >= 0 {
                if self
                    .state
                    .compare_exchange_weak(s, s + 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return RwLockReadGuard { lock: self };
                }
                continue;
            }
            // Writer active, wait.
            self.waiters
                .wait_until(|| self.state.load(Ordering::Acquire) >= 0);
        }
    }

    /// Acquire an exclusive write lock. Blocks if any readers or a writer are active.
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        loop {
            if self
                .state
                .compare_exchange_weak(0, -1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return RwLockWriteGuard { lock: self };
            }
            // Readers or writer active, wait.
            self.waiters
                .wait_until(|| self.state.load(Ordering::Acquire) == 0);
        }
    }

    /// Acquire a shared read lock, pushing `rank` onto the per-thread
    /// lock-rank stack. The rank is popped when the returned `RankedGuard`
    /// is dropped, AFTER the inner read guard is released. Zero-cost in
    /// release builds.
    #[allow(dead_code)]
    pub fn read_ranked(
        &self,
        rank: u16,
        site: &'static str,
    ) -> RankedGuard<RwLockReadGuard<'_, T>> {
        crate::debug::lock_order::enter(rank, site);
        let inner = self.read();
        RankedGuard::new(inner, rank, site)
    }

    /// Acquire an exclusive write lock, pushing `rank` onto the per-thread
    /// lock-rank stack. The rank is popped when the returned `RankedGuard`
    /// is dropped, AFTER the inner write guard is released. Zero-cost in
    /// release builds.
    #[allow(dead_code)]
    pub fn write_ranked(
        &self,
        rank: u16,
        site: &'static str,
    ) -> RankedGuard<RwLockWriteGuard<'_, T>> {
        crate::debug::lock_order::enter(rank, site);
        let inner = self.write();
        RankedGuard::new(inner, rank, site)
    }

    /// Get mutable access when the lock itself is uniquely borrowed.
    #[expect(unused)]
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.value.get() }
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut dbg = f.debug_struct("RwLock");
        let s = self.state.load(Ordering::Relaxed);
        match s {
            -1 => {
                dbg.field("value", &"<write-locked>");
            }
            0 => {
                let val = unsafe { &*self.value.get() };
                dbg.field("value", val);
            }
            n => {
                dbg.field("value", &alloc::format!("<read-locked, {} readers>", n));
            }
        }
        dbg.finish()
    }
}

// -- Read guard ---------------------------------------------------------------

pub struct RwLockReadGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        let prev = self.lock.state.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "RwLock read underflow");
        if prev == 1 {
            // Last reader, wake a waiting writer.
            self.lock.waiters.wake_one();
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

// -- Write guard --------------------------------------------------------------

pub struct RwLockWriteGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        let prev = self.lock.state.swap(0, Ordering::Release);
        debug_assert_eq!(prev, -1, "RwLock write released while not write-locked");
        // Wake all waiters -- both readers and writers can proceed to compete.
        self.lock.waiters.wake_all();
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}
