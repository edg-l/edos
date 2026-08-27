use core::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
};

use crate::{
    debug::lock_order::RankedGuard,
    thread::{
        scheduler::{current_thread, current_thread_id},
        thread::{ThreadId, get_thread_by_id},
        waitqueue::WaitQueue,
    },
};

/// How many read holders one lock can name, and so how many a blocked writer
/// can lend to. Sized to `make run-big`'s 16 CPUs: a reader that never blocks
/// occupies a slot only while it is on a CPU, so this covers every read holder
/// on a machine of that size and a reader beyond it goes unrecorded rather than
/// displacing one.
const READER_SLOTS: usize = 16;

/// A [`RwLockReadGuard`] that holds no slot, because the lock was unarmed when
/// it was taken or every slot was already spoken for.
const NO_SLOT: usize = usize::MAX;

/// The holders a waiter lent to, kept so it can take the loans back.
///
/// A fixed array rather than a `Vec`: this is built on the way into a park, and
/// allocating there would put the frame allocator's lock under a lock nobody
/// ranked it against.
struct Loans {
    ids: [u64; READER_SLOTS],
    len: usize,
}

impl Loans {
    const fn none() -> Self {
        Self {
            ids: [0; READER_SLOTS],
            len: 0,
        }
    }

    fn push(&mut self, id: u64) {
        if self.len < READER_SLOTS {
            self.ids[self.len] = id;
            self.len += 1;
        }
    }

    /// End every loan in the list.
    ///
    /// [`Thread::drop_lent_priority`] ends *all* of a thread's loans, not this
    /// one, so a second waiter on the same lock loses the loan it made at the
    /// same time. It re-lends on its next turn round the acquire loop, which is
    /// the same forfeit `BlockingMutex` documents and takes for the same
    /// reason: a loan is a priority, not a count.
    ///
    /// [`Thread::drop_lent_priority`]: crate::thread::thread::Thread::drop_lent_priority
    fn take_back(&self) {
        for &id in &self.ids[..self.len] {
            if let Some(thread) = get_thread_by_id(ThreadId(id)) {
                thread.drop_lent_priority();
            }
        }
    }
}

/// A reader-writer lock that blocks waiting threads via the scheduler.
///
/// Multiple readers can hold the lock concurrently. A writer has exclusive
/// access. Waiting threads park (yield to the scheduler) rather than spin.
///
/// State encoding (AtomicI32):
///   0        = unlocked
///   positive = number of active readers
///   -1       = writer held
///
/// # Priority inheritance
///
/// A waiter lends the holders its priority for as long as it waits, so an
/// unrelated thread of middling importance cannot keep a holder off the CPU and
/// thereby set how long the most important thread on the machine waits. See
/// [`BlockingMutex`] for the shape of the inversion and the numbers.
///
/// **The loan is undone by the waiter, not by the release.** `BlockingMutex`
/// ends its loan in `release` because it has exactly one holder to name there;
/// a lock whose holders may be a *set* has no such point, and a release that
/// tried to end loans made to other readers would be ending loans that are
/// still owed. Taking the loan back where it was made needs no record on the
/// lock and cannot strand one on a thread that has since let go.
///
/// # What a reader is lent, and what arming is for
///
/// A writer waiting behind readers has to name them, and the only place that
/// can be recorded is the acquisition. Paying for that on every read would tax
/// the two hottest read paths in the kernel — `SCHEDULERS`, read on every
/// placement, and the VFS mount table — for a case most of these locks never
/// see, since almost every writer in the tree is a boot-time or
/// driver-registration one.
///
/// So recording is **armed**: [`RwLock::readers`] stays empty, and a read pays
/// one relaxed load, until the first writer actually blocks on this lock. From
/// then on readers claim a slot and a blocked writer can lend to them. Arming
/// is edge-triggered, so the writer that arms a lock lends to the readers it is
/// waiting on only once they cycle; every writer after it is covered from the
/// start. That is the whole cost of keeping the unarmed path free.
///
/// [`BlockingMutex`]: crate::thread::mutex::BlockingMutex
pub struct RwLock<T> {
    state: AtomicI32,
    /// The write holder's [`ThreadId`], 0 when the lock is not write-held.
    ///
    /// Published after the state is taken and cleared before it is given up, so
    /// a waiter that reads an owner here saw one that really held it.
    ///
    /// [`ThreadId`]: crate::thread::thread::ThreadId
    owner: AtomicU64,
    /// Whether read holders record themselves. Set by the first writer to
    /// block; never cleared, since a lock that has been written under
    /// contention once will be again.
    pi_armed: AtomicBool,
    /// [`ThreadId`]s of the read holders, 0 for a free slot. Populated only
    /// while [`RwLock::pi_armed`].
    ///
    /// [`ThreadId`]: crate::thread::thread::ThreadId
    readers: [AtomicU64; READER_SLOTS],
    waiters: WaitQueue,
    value: UnsafeCell<T>,
}

// SAFETY: every field but `value` is an atomic or a `WaitQueue`, and `value`
// is only reached through a guard the lock hands out -- one writer alone, or
// readers with no writer. Moving the lock moves the `T` inside it, so `Send`
// asks only `T: Send`; sharing it lends `&T` to several readers at once, so
// `Sync` additionally asks `T: Sync`.
unsafe impl<T: Send> Send for RwLock<T> {}
// SAFETY: the same argument as the impl above.
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicI32::new(0),
            owner: AtomicU64::new(0),
            pi_armed: AtomicBool::new(false),
            readers: [const { AtomicU64::new(0) }; READER_SLOTS],
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
                    return RwLockReadGuard {
                        lock: self,
                        slot: self.claim_reader_slot(),
                    };
                }
                continue;
            }
            // Writer active, wait. A reader can only ever be blocked by the one
            // write holder, so this is the single-owner case and needs none of
            // the reader bookkeeping below.
            let loans = self.lend_to_writer();
            self.waiters
                .wait_until(|| self.state.load(Ordering::Acquire) >= 0);
            loans.take_back();
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
                if let Some(tid) = current_thread_id() {
                    self.owner.store(tid.0, Ordering::Release);
                }
                return RwLockWriteGuard { lock: self };
            }
            // Readers or writer active, wait.
            let loans = self.lend_to_holders();
            self.waiters
                .wait_until(|| self.state.load(Ordering::Acquire) == 0);
            loans.take_back();
        }
    }

    /// Record the calling thread as a read holder, once this lock is armed.
    ///
    /// The probe starts at the thread's own id so a given thread lands in the
    /// same slot each time and the common case is one uncontended
    /// compare-exchange. A reader that finds every slot taken is simply not
    /// recorded: it forfeits a loan it might have been lent, and never holds a
    /// slot a live reader needs.
    fn claim_reader_slot(&self) -> usize {
        if !self.pi_armed.load(Ordering::Relaxed) {
            return NO_SLOT;
        }
        let Some(tid) = current_thread_id() else {
            return NO_SLOT;
        };
        let start = (tid.0 as usize) % READER_SLOTS;
        for i in 0..READER_SLOTS {
            let slot = (start + i) % READER_SLOTS;
            if self.readers[slot]
                .compare_exchange(0, tid.0, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return slot;
            }
        }
        NO_SLOT
    }

    /// Lend the calling thread's priority to the write holder, if there is one.
    fn lend_to_writer(&self) -> Loans {
        let mut loans = Loans::none();
        let Some(me) = current_thread() else {
            return loans;
        };
        let prio = me.effective_priority();
        let owner = self.owner.load(Ordering::Acquire);
        if owner != 0
            && owner != me.id.0
            && let Some(holder) = get_thread_by_id(ThreadId(owner))
        {
            holder.lend_priority(prio);
            loans.push(owner);
        }
        loans
    }

    /// Lend the calling thread's priority to whoever holds the lock: the one
    /// write holder, or every read holder this lock has recorded.
    fn lend_to_holders(&self) -> Loans {
        if self.state.load(Ordering::Acquire) < 0 {
            return self.lend_to_writer();
        }

        // Read-held. Arm first, so that readers arriving from here on record
        // themselves even if this attempt finds none to lend to.
        self.pi_armed.store(true, Ordering::Relaxed);

        let mut loans = Loans::none();
        let Some(me) = current_thread() else {
            return loans;
        };
        let prio = me.effective_priority();
        for slot in &self.readers {
            let id = slot.load(Ordering::Acquire);
            if id != 0
                && id != me.id.0
                && let Some(holder) = get_thread_by_id(ThreadId(id))
            {
                holder.lend_priority(prio);
                loans.push(id);
            }
        }
        loans
    }

    /// Acquire a shared read lock, pushing `rank` onto the per-thread
    /// lock-rank stack. The rank is popped when the returned `RankedGuard`
    /// is dropped, AFTER the inner read guard is released. Zero-cost in
    /// release builds.
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
    pub fn write_ranked(
        &self,
        rank: u16,
        site: &'static str,
    ) -> RankedGuard<RwLockWriteGuard<'_, T>> {
        crate::debug::lock_order::enter(rank, site);
        let inner = self.write();
        RankedGuard::new(inner, rank, site)
    }

    /// Acquire an exclusive write lock for a same-class different-instance
    /// acquisition. Permits `rank >= top` rather than strictly greater.
    /// Use ONLY in key-ordered same-class patterns (e.g. `vfs::rename` with
    /// two parent inodes). NOT reentrance protection.
    pub fn write_ranked_same(
        &self,
        rank: u16,
        site: &'static str,
    ) -> RankedGuard<RwLockWriteGuard<'_, T>> {
        crate::debug::lock_order::enter_same(rank, site);
        let inner = self.write();
        RankedGuard::new(inner, rank, site)
    }

    /// Get mutable access when the lock itself is uniquely borrowed.
    #[expect(unused, reason = "the uncontended accessor every lock type offers")]
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
    /// The [`RwLock::readers`] slot this holder claimed, or [`NO_SLOT`].
    slot: usize,
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        // Give the slot up before the count, so a writer that sees the last
        // reader leave never then reads this thread out of the table and lends
        // to a thread that is no longer holding anything.
        if self.slot != NO_SLOT {
            self.lock.readers[self.slot].store(0, Ordering::Release);
        }
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
        // Unpublish before the state, so the next holder's own store is never
        // overwritten by this one's clear.
        self.lock.owner.store(0, Ordering::Release);
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
