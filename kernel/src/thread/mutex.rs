use core::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::thread::waitqueue::WaitQueue;

/// A mutex that blocks waiting threads instead of spinning.
///
/// Threads that fail to acquire the lock enqueue themselves onto an internal
/// wait queue and yield to the scheduler. When the lock holder releases the
/// mutex, one waiter is woken and competes to take ownership.
pub struct BlockingMutex<T> {
    locked: AtomicBool,
    waiters: WaitQueue,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for BlockingMutex<T> {}
unsafe impl<T: Send> Sync for BlockingMutex<T> {}

impl<T> BlockingMutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            waiters: WaitQueue::new(),
            value: UnsafeCell::new(value),
        }
    }

    /// Attempt to take the lock without blocking.
    pub fn try_lock(&self) -> Option<BlockingMutexGuard<'_, T>> {
        if self.try_acquire() {
            Some(BlockingMutexGuard { lock: self })
        } else {
            None
        }
    }

    /// Acquire the lock, blocking the current thread until it becomes available.
    pub fn lock(&self) -> BlockingMutexGuard<'_, T> {
        loop {
            if self.try_acquire() {
                return BlockingMutexGuard { lock: self };
            }
            let _ = self
                .waiters
                .wait_until(|| !self.locked.load(Ordering::Acquire));
        }
    }

    #[inline]
    fn try_acquire(&self) -> bool {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    #[inline]
    fn release(&self) {
        let was_locked = self.locked.swap(false, Ordering::Release);
        debug_assert!(was_locked, "BlockingMutex released while unlocked");
        let _ = self.waiters.wake_one();
    }

    /// Get mutable access when the mutex itself is uniquely borrowed.
    #[expect(unused)]
    pub fn get_mut(&mut self) -> &mut T {
        // Safe because &mut self guarantees unique access to the inner value.
        unsafe { &mut *self.value.get() }
    }

    /// Check whether the lock is currently held.
    #[expect(unused)]
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }
}

impl<T: fmt::Debug> fmt::Debug for BlockingMutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut dbg = f.debug_struct("BlockingMutex");
        if let Some(guard) = self.try_lock() {
            dbg.field("value", &*guard);
            drop(guard);
        } else {
            dbg.field("value", &"<locked>");
        }
        dbg.finish()
    }
}

pub struct BlockingMutexGuard<'a, T> {
    lock: &'a BlockingMutex<T>,
}

impl<'a, T> Deref for BlockingMutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // Safe: the guard represents exclusive access to the inner value.
        unsafe { &*self.lock.value.get() }
    }
}

impl<'a, T> DerefMut for BlockingMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<'a, T> Drop for BlockingMutexGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.release();
    }
}
