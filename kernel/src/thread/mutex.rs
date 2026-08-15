use core::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::thread::{
    scheduler::{current_thread, current_thread_id},
    thread::{ThreadId, get_thread_by_id},
    waitqueue::WaitQueue,
};

/// How long [`BlockingMutex::holder`] waits for a fresh holder to publish
/// itself. Sized to cover the two stores between the compare-exchange and the
/// owner being visible, not to wait out a preemption.
const OWNER_PUBLISH_SPINS: u32 = 64;

/// A mutex that blocks waiting threads instead of spinning.
///
/// Threads that fail to acquire the lock enqueue themselves onto an internal
/// wait queue and yield to the scheduler. When the lock holder releases the
/// mutex, one waiter is woken and competes to take ownership.
///
/// # Priority inheritance
///
/// A waiter lends the holder its priority for the length of the section, so a
/// thread of middling importance that wants neither the lock nor anything
/// behind it cannot keep the holder off the CPU and so set how long the most
/// important thread on the machine waits. Without it that wait is set by the
/// unrelated thread rather than by the section: the `prio-inversion` case in
/// `thread/sched_test.rs` measured a 10 ms section blocking the top-priority
/// waiter for 181 ms, and `doc/SCHED-ROADMAP.md` carries the numbers.
///
/// The loan is a priority, not a count: [`Thread::lend_priority`] raises and
/// [`Thread::drop_lent_priority`] ends every loan at once. A thread holding two
/// of these therefore gives up an outer loan when it releases the inner lock.
/// That forfeits inheritance it was owed; it never grants any that was not, and
/// paying for the exact case costs the holder a list of what it holds on a path
/// where the uncontended cost is what matters.
///
/// [`Thread::lend_priority`]: crate::thread::thread::Thread::lend_priority
/// [`Thread::drop_lent_priority`]: crate::thread::thread::Thread::drop_lent_priority
pub struct BlockingMutex<T> {
    locked: AtomicBool,
    /// The holder's [`ThreadId`], 0 when free. Published after the lock is
    /// taken and cleared before it is dropped, so a waiter that reads a holder
    /// here saw one that really held it.
    ///
    /// [`ThreadId`]: crate::thread::thread::ThreadId
    owner: AtomicU64,
    /// The [`ThreadId`] a loan was published for, 0 when none.
    ///
    /// It names the holder rather than merely recording that a loan exists, so
    /// a release can tell a loan made to *itself* from one a waiter published a
    /// moment too late for a holder that has already gone. Ending the latter
    /// would cancel whatever inheritance the current holder is genuinely owed
    /// on some other lock, which is a live loan silently lost.
    ///
    /// [`ThreadId`]: crate::thread::thread::ThreadId
    lent_to: AtomicU64,
    waiters: WaitQueue,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for BlockingMutex<T> {}
unsafe impl<T: Send> Sync for BlockingMutex<T> {}

impl<T> BlockingMutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            owner: AtomicU64::new(0),
            lent_to: AtomicU64::new(0),
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
            debug_assert!(
                x86_64::instructions::interrupts::are_enabled(),
                "BlockingMutex::lock contended with interrupts disabled"
            );
            self.lend_to_holder();
            let _ = self
                .waiters
                .wait_until(|| !self.locked.load(Ordering::Acquire));
        }
    }

    #[inline]
    fn try_acquire(&self) -> bool {
        let taken = self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        if taken && let Some(tid) = current_thread_id() {
            self.owner.store(tid.0, Ordering::Release);
        }
        taken
    }

    /// The current holder, waiting out the gap between a successful
    /// acquisition and the owner being published.
    ///
    /// [`BlockingMutex::try_acquire`] cannot publish the owner atomically with
    /// the compare-exchange, so a waiter arriving between the two reads zero.
    /// Parking on that reading would forfeit the loan for the whole section,
    /// because the next chance to lend is the wake that only a release sends.
    /// The gap is a couple of instructions wide and a holder that is not
    /// published within the bound is simply missed, as it was before.
    fn holder(&self) -> Option<ThreadId> {
        for _ in 0..OWNER_PUBLISH_SPINS {
            let owner = self.owner.load(Ordering::Acquire);
            if owner != 0 {
                return Some(ThreadId(owner));
            }
            // Released while we looked: the caller's loop re-attempts the
            // acquisition, so there is nobody to lend to.
            if !self.locked.load(Ordering::Acquire) {
                return None;
            }
            core::hint::spin_loop();
        }
        None
    }

    /// Lend the calling thread's priority to whoever holds the lock.
    ///
    /// The loan is what the *waiter* is served at rather than its own static
    /// priority, so a chain of holders each blocked on the next carries the
    /// donation down it one link per acquisition.
    fn lend_to_holder(&self) {
        let Some(me) = current_thread() else { return };
        let prio = me.effective_priority();
        let Some(owner) = self.holder() else { return };
        if owner == me.id {
            return;
        }
        let Some(holder) = get_thread_by_id(owner) else {
            return;
        };
        holder.lend_priority(prio);
        // Publish the loan before re-reading the owner, so that a release
        // racing this either sees the loan and ends it, or is seen by the
        // re-read below and the loan is taken back here. Ordering it the other
        // way leaves a window where neither side ends it.
        self.lent_to.store(owner.0, Ordering::Release);

        // The holder may have released between the read above and the loan, in
        // which case the loan is on a thread that owes this waiter nothing.
        // Take it back rather than leave it running heavy; a thread that took
        // and re-took the same lock reads as unchanged here and keeps it,
        // which is correct because it does hold the lock.
        if self.owner.load(Ordering::Acquire) != owner.0 {
            holder.drop_lent_priority();
            // Withdraw the record too, but only while it is still ours: a new
            // holder may already have a waiter of its own behind it.
            let _ = self
                .lent_to
                .compare_exchange(owner.0, 0, Ordering::AcqRel, Ordering::Relaxed);
        }
    }

    #[inline]
    fn release(&self) {
        let owner = self.owner.swap(0, Ordering::AcqRel);
        // Only a loan published for *this* holder is ours to end. One naming
        // anybody else belongs to an owner that has already gone, and ending it
        // here would drop whatever this thread is owed on another lock.
        if owner != 0
            && self.lent_to.swap(0, Ordering::AcqRel) == owner
            && let Some(me) = current_thread()
        {
            me.drop_lent_priority();
        }
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
