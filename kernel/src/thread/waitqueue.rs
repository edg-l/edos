use alloc::sync::Weak;
use core::sync::atomic::{AtomicUsize, Ordering, fence};
use core::time::Duration;
use heapless::Deque;
use spin::Mutex;
use x86_64::instructions::interrupts::{self, without_interrupts};

use crate::thread::scheduler::{
    current_thread_killed, current_thread_weak, thread_park_while, thread_sleep,
};
use crate::thread::{
    scheduler::{WakePriority, sched},
    thread::Thread,
};
use crate::timer::Instant;

/// Maximum number of threads that can wait on a single WaitQueue.
/// A fault storm on a many-core boot can bring one thread per CPU, plus the
/// readers already queued, to the same uncached page at once, so the bound has
/// to clear the core count with room to spare. Cost: ~512 B per handle.
const WAITQUEUE_CAP: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Condition became ready without blocking.
    Ready,
    /// Thread was parked and later woken.
    Parked,
    /// Thread slept until the timeout elapsed without the condition becoming ready.
    TimedOut,
    /// The waiting thread was killed. Only [`WaitQueue::wait_until_killable`]
    /// reports this; every other wait ignores the flag and parks on.
    Killed,
}

#[derive(Debug)]
pub struct WaitQueue {
    inner: Mutex<Deque<Weak<Thread>, WAITQUEUE_CAP>>,
    /// Number of handles currently enrolled in `inner`, published so a
    /// producer can skip the wake path entirely when nobody is waiting.
    ///
    /// Written to `inner.len()` while `inner` is held, so it is exact rather
    /// than a hint. See [`WaitQueue::has_waiters`] for the ordering the read
    /// side has to establish before it can be trusted.
    waiters: AtomicUsize,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(Deque::new()),
            waiters: AtomicUsize::new(0),
        }
    }

    /// Put the current thread to sleep until woken.
    ///
    /// **`ready` must not block.** It is evaluated inside
    /// `without_interrupts` to close the enqueue-versus-wake window, so a
    /// predicate that acquires a contended `BlockingMutex` trips that
    /// primitive's interrupts-enabled assertion and panics the kernel. Read
    /// atomics, or probe a lock with `try_lock` and bias the unavailable case
    /// towards "ready" so the caller re-checks under the real lock.
    pub fn wait_until<F: Fn() -> bool>(&self, ready: F) -> WaitOutcome {
        self.wait_internal(ready, None, false)
    }

    /// Wait until `ready`, or until the waiting thread is killed.
    ///
    /// A syscall that blocks indefinitely on something only a peer can supply
    /// must use this. `kill` marks the thread and wakes it, but the death
    /// itself happens at the syscall return boundary, so a wait that re-parks
    /// on a predicate the peer will never satisfy never reaches that boundary
    /// and the process cannot be killed at all — not even by `SIGKILL`. A
    /// server parked in `accept` with no client coming is the case that found
    /// this.
    ///
    /// The caller returns as soon as this reports [`WaitOutcome::Killed`];
    /// what it returns does not matter, since the thread dies before the value
    /// reaches userspace.
    ///
    /// This is opt-in because aborting a wait is only safe where the caller
    /// can abandon what it was waiting for. A page fill or a journal commit
    /// cannot, and those keep parking.
    ///
    /// A pending *stop* is deliberately not handled here: `SIGTSTP` on a
    /// blocked call should suspend and then resume the call, which needs
    /// restart semantics this kernel does not have.
    ///
    /// `ready` carries the same non-blocking requirement as [`wait_until`].
    ///
    /// [`wait_until`]: WaitQueue::wait_until
    pub fn wait_until_killable<F: Fn() -> bool>(&self, ready: F) -> WaitOutcome {
        self.wait_internal(ready, None, true)
    }

    /// Put the current thread to sleep until woken or the timeout elapses.
    ///
    /// `ready` carries the same non-blocking requirement as [`wait_until`].
    ///
    /// [`wait_until`]: WaitQueue::wait_until
    pub fn wait_until_timeout<F: Fn() -> bool>(
        &self,
        ready: F,
        timeout: Option<Duration>,
    ) -> WaitOutcome {
        self.wait_internal(ready, timeout, false)
    }

    /// Wake one live thread from the queue.
    ///
    /// Pops entries until a live thread (Weak::upgrade succeeds) is woken or
    /// the queue is empty. Dead entries (thread exited between enrollment and
    /// wake) are silently discarded so a single dead waiter at head cannot
    /// stall the wake.
    pub fn wake_one(&self) -> bool {
        if !self.has_waiters() {
            return false;
        }
        loop {
            let handle_opt = without_interrupts(|| {
                let mut q = self.inner.lock();
                let h = q.pop_front();
                self.publish(&q);
                h
            });
            match handle_opt {
                None => return false,
                Some(handle) => {
                    if sched().wake_thread(&handle, WakePriority::Normal) {
                        return true;
                    }
                    // Thread exited between enrollment and wake, or self-skip
                    // fired — discard and try the next entry.
                }
            }
        }
    }

    /// Wake all threads
    pub fn wake_all(&self) -> usize {
        if !self.has_waiters() {
            return 0;
        }
        // Drain into stack buffer under lock (heapless::Vec, no allocation),
        // then wake outside to avoid holding the lock while do_wake spins.
        let handles: heapless::Vec<Weak<Thread>, WAITQUEUE_CAP> = without_interrupts(|| {
            let mut q = self.inner.lock();
            let mut v = heapless::Vec::new();
            while let Some(h) = q.pop_front() {
                let _ = v.push(h);
            }
            self.publish(&q);
            v
        });
        let mut n = 0usize;
        for handle in &handles {
            if sched().wake_thread(handle, WakePriority::Normal) {
                n += 1;
            }
        }
        n
    }

    /// [`WaitQueue::wake_all`], from an interrupt handler.
    ///
    /// Differs only in reaching the scheduler through `wake_thread_irq`, whose
    /// wake protocol is documented safe from any context. The queue's own lock
    /// is taken under `without_interrupts` by every waiter, so a handler cannot
    /// arrive while a CPU holds it and deadlock against itself.
    pub fn wake_all_irq(&self) -> usize {
        if !self.has_waiters() {
            return 0;
        }
        let handles: heapless::Vec<Weak<Thread>, WAITQUEUE_CAP> = without_interrupts(|| {
            let mut q = self.inner.lock();
            let mut v = heapless::Vec::new();
            while let Some(h) = q.pop_front() {
                let _ = v.push(h);
            }
            self.publish(&q);
            v
        });
        let mut n = 0usize;
        for handle in &handles {
            if sched().wake_thread_irq(handle, WakePriority::Interrupt) {
                n += 1;
            }
        }
        n
    }

    /// Check whether the queue currently has any waiters, without taking the
    /// lock. A producer calls this before doing anything that only a waiter
    /// would care about.
    ///
    /// The fence is load-bearing and a plain `SeqCst` load will not do: on x86
    /// that lowers to a bare `mov`, since the barrier rides on `SeqCst` stores.
    /// Without it the store buffer may still hold the producer's publication
    /// when the count is read, while the waiter side is already fenced by
    /// `inner.lock()` and [`WaitQueue::publish`]. One side ordered and the other
    /// not is the store-buffer litmus: both can read stale, the producer skips
    /// the wake, and the park here never ends.
    ///
    /// It belongs here rather than at each producer, because `inner.lock()` used
    /// to supply the barrier for free and every caller was written without one.
    /// Linux draws the same line with `wq_has_sleeper()`, which is `smp_mb()`
    /// plus `waitqueue_active()`.
    pub fn has_waiters(&self) -> bool {
        fence(Ordering::SeqCst);
        self.waiters.load(Ordering::SeqCst) != 0
    }

    /// Check whether the queue currently has any waiters.
    pub fn is_empty(&self) -> bool {
        !self.has_waiters()
    }

    /// Publish the queue length. Must be called with `inner` held, and before
    /// an enrolling waiter re-checks its predicate.
    fn publish(&self, q: &Deque<Weak<Thread>, WAITQUEUE_CAP>) {
        self.waiters.store(q.len(), Ordering::SeqCst);
    }

    /// Single-iteration wait: push handle → park (or sleep) → remove handle.
    ///
    /// **Protocol invariant**: each call performs exactly one push + one park
    /// + one remove. The producer's `wake_one`/`wake_all` pops the handle as
    ///   part of waking it, so the post-park `retain` is a no-op in the common
    ///   case (spurious-pop self-remove safety only).
    ///
    /// Because `thread_park_while` may return spuriously (Rust std-style
    /// contract — see CLAUDE.md "Park/wake protocol"), this function may
    /// also return spuriously. Callers that must block until a real
    /// condition is met (e.g. `BlockingMutex::lock`) MUST loop on that
    /// condition externally — re-entering `wait_until` re-pushes the handle
    /// and the protocol stays consistent. Looping inside this function (or
    /// inside `thread_park_while`) would re-park without re-pushing and
    /// silently lose wakes when the producer churns the lock faster than
    /// the waiter can drain the queue.
    fn wait_internal<F: Fn() -> bool>(
        &self,
        ready: F,
        timeout: Option<Duration>,
        killable: bool,
    ) -> WaitOutcome {
        if ready() {
            return WaitOutcome::Ready;
        }
        if killable && current_thread_killed() {
            return WaitOutcome::Killed;
        }

        #[derive(Copy, Clone)]
        enum SleepAction {
            Park,
            Sleep(Duration),
        }

        let my_handle = current_thread_weak().unwrap();
        let mut action: Option<SleepAction> = None;

        // Enqueue and check readiness inside without_interrupts to close the
        // lost-wakeup window: heapless::Deque does not allocate, and
        // Weak::clone is a refcount bump only — both are safe here.
        // Park/sleep must happen OUTSIDE without_interrupts since context
        // switching requires interrupts to be re-enabled.
        interrupts::without_interrupts(|| {
            {
                let mut q = self.inner.lock();
                q.push_back(my_handle.clone())
                    .expect("WaitQueue overflow: too many waiters");
                self.publish(&q);
            }

            if ready() {
                let mut q = self.inner.lock();
                q.retain(|w| !Weak::ptr_eq(w, &my_handle));
                self.publish(&q);
                return;
            }

            action = Some(match timeout {
                Some(dt) => SleepAction::Sleep(dt),
                None => SleepAction::Park,
            });
        });

        // Perform the actual park/sleep with interrupts enabled.
        // thread_park_while sets Parked before checking the closure, so a waker
        // that fires after IRQs re-enable but before park will still succeed.
        // The timed variant must honour its deadline: `thread_sleep` returns on
        // any wake, including a token left by an earlier wait, so sleeping once
        // told a caller that asked for five seconds that it had timed out
        // microseconds later.
        //
        // The untimed arm deliberately still parks once and returns. Callers
        // re-check their own condition and call again; looping here instead
        // blocks a thread whose predicate is only made true by work that thread
        // has yet to do, which stalls the boot.
        if let Some(chosen) = action {
            match chosen {
                SleepAction::Park => {
                    thread_park_while(|| !(ready() || killable && current_thread_killed()));
                }
                SleepAction::Sleep(dt) => {
                    let deadline = Instant::now() + dt;
                    loop {
                        let now = Instant::now();
                        if ready() || now >= deadline {
                            break;
                        }
                        thread_sleep(deadline.duration_since(now));

                        // A wake pops the waiter off the queue, so waking
                        // without the predicate being true leaves this thread
                        // unregistered: every later wake would miss it and it
                        // would sleep out its whole deadline before noticing.
                        // Re-enrol, then re-check, so a wake landing between
                        // the two is not lost either.
                        let done = interrupts::without_interrupts(|| {
                            let mut q = self.inner.lock();
                            if q.iter().all(|w| !Weak::ptr_eq(w, &my_handle)) {
                                q.push_back(my_handle.clone())
                                    .expect("WaitQueue overflow: too many waiters");
                                self.publish(&q);
                            }
                            drop(q);
                            ready()
                        });
                        if done {
                            break;
                        }
                    }
                }
            }
        }

        // Always remove our handle from the wait queue after waking,
        // regardless of how we were woken (park, sleep, or timeout).
        interrupts::without_interrupts(|| {
            let mut q = self.inner.lock();
            q.retain(|w| !Weak::ptr_eq(w, &my_handle));
            self.publish(&q);
        });

        if killable && current_thread_killed() {
            return WaitOutcome::Killed;
        }

        let Some(action) = action else {
            return WaitOutcome::Ready;
        };

        match action {
            // A park reports `Parked` whether or not the condition now holds,
            // so do not pay for a predicate evaluation nobody reads.
            SleepAction::Park => WaitOutcome::Parked,
            SleepAction::Sleep(_) if ready() => WaitOutcome::Parked,
            SleepAction::Sleep(_) => WaitOutcome::TimedOut,
        }
    }
}
