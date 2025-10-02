use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use crate::sys::{
    Errno,
    calls::{syscall2, syscall3},
    constants::{SYS_FUTEX_WAIT, SYS_FUTEX_WAKE},
    errno,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FutexWaitResult {
    /// The caller slept and was explicitly woken.
    Woken,
    /// The futex value no longer matched the expected value; caller should retry in userspace.
    NotReady,
    /// The futex wait timed out before the value changed.
    TimedOut,
}

/// Wait on a userspace futex word until it differs from `expected` or another thread wakes us.
///
/// Returns [`FutexWaitResult::NotReady`] when the value changed (or a spurious wake occurred) and
/// no blocking happened, [`FutexWaitResult::Woken`] when the thread slept and was explicitly woken,
/// and [`Err`] if the syscall failed.
pub fn futex_wait(
    addr: &AtomicU32,
    expected: u32,
    timeout: Option<Duration>,
) -> Result<FutexWaitResult, Errno> {
    let ptr = addr as *const AtomicU32 as *const u32;
    let timeout_ns = match timeout {
        Some(duration) => {
            let nanos = duration.as_nanos();
            if nanos >= u64::MAX as u128 {
                u64::MAX - 1
            } else {
                nanos as u64
            }
        }
        None => u64::MAX,
    };
    let result = unsafe { syscall3(SYS_FUTEX_WAIT, ptr as u64, expected as u64, timeout_ns) };

    if result == u64::MAX {
        Err(errno())
    } else if result == 0 {
        Ok(FutexWaitResult::Woken)
    } else if result == 2 {
        Ok(FutexWaitResult::TimedOut)
    } else {
        Ok(FutexWaitResult::NotReady)
    }
}

/// Wake up to `count` threads sleeping on the futex word at `addr`.
///
/// Returns the number of threads woken on success or [`Err`] when the syscall fails.
pub fn futex_wake(addr: &AtomicU32, count: u32) -> Result<u32, Errno> {
    let ptr = addr as *const AtomicU32 as *const u32;
    let result = unsafe { syscall2(SYS_FUTEX_WAKE, ptr as u64, count as u64) };

    if result == u64::MAX {
        Err(errno())
    } else {
        Ok(result as u32)
    }
}

#[derive(Debug)]
pub struct Mutex<T> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        loop {
            if self
                .state
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return MutexGuard { mutex: self };
            }

            while self.state.load(Ordering::Acquire) != 0 {
                match futex_wait(&self.state, 1, None) {
                    Ok(FutexWaitResult::Woken)
                    | Ok(FutexWaitResult::NotReady)
                    | Ok(FutexWaitResult::TimedOut) => {}
                    Err(_) => break,
                }
                spin_loop();
            }
        }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        if self
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(MutexGuard { mutex: self })
        } else {
            None
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.value.get() }
    }

    fn unlock(&self) {
        self.state.store(0, Ordering::Release);
        let _ = futex_wake(&self.state, 1);
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<'a, T> Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<'a, T> DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<'a, T> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<'a, T: core::fmt::Debug> core::fmt::Debug for MutexGuard<'a, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.deref())
    }
}
