# `fsync` panicked the kernel on its own wait predicate

## Status

Fixed by `8992a30`. The invariant is documented on `WaitQueue::wait_until`.

## Symptoms

A `fsbench /var` run panicked the kernel once, in `BlockingMutex::lock`'s
interrupts-enabled assertion. The CPU then stopped acknowledging IPIs and
`tlb_shootdown` panicked on top of it, so the first panic is not necessarily
the one at the top of the log.

Racy, not deterministic — the same suite usually completed. That is the shape
to recognise: the predicate only has to be re-evaluated at the moment another
thread holds the lock.

## Root cause

`Journal::committed_seq` took the `BlockingMutex<JournalState>`, and
`force_commit_and_wait` handed `|| self.committed_seq() >= target_seq` to
`commit_wq.wait_until_timeout`.

`WaitQueue::wait_internal` evaluates its readiness predicate inside
`without_interrupts`, to close the enqueue-versus-wake window. `BlockingMutex::
lock` debug-asserts that interrupts are enabled when it has to block. So an
`fsync` that re-checked its predicate while the committer kthread held `state`
panicked.

Two more call sites had the same shape: the committer kthread's own
`has_pending_work()` predicate, and `Mailbox::recv`, which re-checked its
`BlockingMutex<VecDeque>` under the same rule.

Fixed by making every wait predicate lock-free, not by relaxing the assert:

- `committed_seq` is mirrored into an `AtomicU64` published by
  `set_committed_seq` under the state lock, so the field and its mirror cannot
  drift;
- the committer uses a `try_lock` hint biased towards "there is work";
- `Mailbox` reuses the `try_lock`-based `is_empty` it already had.

## Reasoning rules going forward

- **A wait predicate must not block.** Read an atomic, or probe with `try_lock`
  and bias the unavailable case towards "ready" so the caller re-checks under
  the real lock. Biasing the other way is a missed wakeup.
- **A mirror of locked state must be published under that lock**, or the two
  drift and the predicate reads a value that never existed.
- **An assertion that fires rarely is still load-bearing.** The temptation here
  was to relax the interrupts-enabled assert, which would have converted a rare
  panic into a rare deadlock.

## How to catch a recurrence

Grep for predicates passed to `wait_until` / `wait_until_timeout` that call a
method taking a `BlockingMutex`. The panic message is
`BlockingMutex::lock contended with interrupts disabled` (`thread/mutex.rs`);
if the log ends in a `tlb_shootdown` panic, look further up for the real first
fault.
