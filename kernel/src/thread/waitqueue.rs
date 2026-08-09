use alloc::sync::Weak;
use core::time::Duration;
use heapless::Deque;
use spin::Mutex;
use x86_64::instructions::interrupts::{self, without_interrupts};

use crate::thread::scheduler::{current_thread_weak, thread_park_while, thread_sleep};
use crate::thread::{
    scheduler::{WakePriority, sched},
    thread::Thread,
};
use crate::timer::Instant;

/// Maximum number of threads that can wait on a single WaitQueue.
/// Raised from 32 to 64 (Foundation #5 Task 0.4b): fault-storm / many-core
/// concurrent-reader scenarios may bring up to 64 threads to the same uncached
/// page simultaneously; 32 was too tight. Cost: ~512 B extra per handle.
const WAITQUEUE_CAP: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Condition became ready without blocking.
    Ready,
    /// Thread was parked and later woken.
    Parked,
    /// Thread slept until the timeout elapsed without the condition becoming ready.
    TimedOut,
}

#[derive(Debug)]
pub struct WaitQueue {
    inner: Mutex<Deque<Weak<Thread>, WAITQUEUE_CAP>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(Deque::new()),
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
        self.wait_internal(ready, None)
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
        self.wait_internal(ready, timeout)
    }

    /// Wake one live thread from the queue.
    ///
    /// Pops entries until a live thread (Weak::upgrade succeeds) is woken or
    /// the queue is empty. Dead entries (thread exited between enrollment and
    /// wake) are silently discarded so a single dead waiter at head cannot
    /// stall the wake.
    pub fn wake_one(&self) -> bool {
        loop {
            let handle_opt = without_interrupts(|| {
                let mut q = self.inner.lock();
                q.pop_front()
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
        // Drain into stack buffer under lock (heapless::Vec, no allocation),
        // then wake outside to avoid holding the lock while do_wake spins.
        let handles: heapless::Vec<Weak<Thread>, WAITQUEUE_CAP> = without_interrupts(|| {
            let mut q = self.inner.lock();
            let mut v = heapless::Vec::new();
            while let Some(h) = q.pop_front() {
                let _ = v.push(h);
            }
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

    /// Check whether the queue currently has any waiters.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// Single-iteration wait: push handle → park (or sleep) → remove handle.
    ///
    /// **Protocol invariant**: each call performs exactly one push + one park
    /// + one remove. The producer's `wake_one`/`wake_all` pops the handle as
    /// part of waking it, so the post-park `retain` is a no-op in the common
    /// case (spurious-pop self-remove safety only).
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
    fn wait_internal<F: Fn() -> bool>(&self, ready: F, timeout: Option<Duration>) -> WaitOutcome {
        if ready() {
            return WaitOutcome::Ready;
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
            }

            if ready() {
                let mut q = self.inner.lock();
                q.retain(|w| !Weak::ptr_eq(w, &my_handle));
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
                    thread_park_while(|| !ready());
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
        });

        let Some(action) = action else {
            return WaitOutcome::Ready;
        };

        if ready() {
            return WaitOutcome::Parked;
        }

        match action {
            SleepAction::Park => WaitOutcome::Parked,
            SleepAction::Sleep(_) => WaitOutcome::TimedOut,
        }
    }
}
