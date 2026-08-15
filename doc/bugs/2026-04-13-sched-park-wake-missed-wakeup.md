# FIXED 2026-04-13 — Missed-wakeup in scheduler park/wake (3 iterations)

## Status
Fixed. Took three iterations to land correctly:

1. `94946c8` — wake-pending token (closes the lost-wake race at the
   primitive level).
2. `956b820` — `thread_park_while` loops on spurious wake. **Wrong fix.**
   Broke the wait-queue protocol; surfaced as a hard hang where
   terminal/taskbar parked on TTY_BUFFER with the lock free and the wait
   queue empty (caught the second time wm fast-path triggered three
   back-to-back eprintlns).
3. `e067ef3` — revert (2)'s loop, fix `sys_waitpid` instead. **Right fix.**
   `thread_park_while` is now "may return spuriously" matching Rust
   std's `Thread::park` contract. Every wait-queue caller already loops;
   `sys_waitpid` was the one bare caller, now wrapped.

Validated: shell + ls/ps/df run cleanly across many commands; full boot
to /bin/sh in ~1.7s (was 4.8s before the speedup work, hung at boot
between iterations 2 and 3).

## Symptoms (all from the same root cause)

- **Original boot hang (~1-in-10)**: all four CPUs idle in
  `Scheduler::run_idle`, terminal/taskbar Parked, no wake en route.
- **Shell exits after a command**: typing `ls` produces output but no
  new prompt. Shell exits cleanly with code 0 right after spawning the
  child. Exposed by iteration (2) — `sys_waitpid` returning 0 with stale
  status because `thread_park_while` returned spuriously, userspace
  `waitpid()` returns -1, shell interprets -1 as "exit shell builtin."
- **Terminal/taskbar deadlock at boot under fast loader**: with both the
  parallel boot loader and the bulk-prefetch landed, wm finishes load
  fast enough to do three back-to-back eprintln calls on TTY_BUFFER. Tid
  19/20 try to acquire between WM's writes, get popped from the wait
  queue by `wake_one`, then iteration (2)'s loop re-parks them without
  re-pushing → next `wake_one` finds an empty queue → wake lost forever.

## Root cause (the design lesson)

There are two valid contracts for a park primitive:

1. **"Loops internally"** — primitive blocks until predicate observes
   false. Caller does not loop. (What iteration 2 implemented.)
2. **"May return spuriously"** — primitive returns whenever woken or
   when an internal hint says go. Caller MUST loop on the predicate.
   (Rust std `Thread::park`. POSIX `pthread_cond_wait`. POSIX `futex_wait`.)

Either is fine in isolation, but **wait-queue protocols require contract
(2)**. The producer's `wake_one` pops the waiter exactly once per wake.
A primitive that re-parks internally (without re-pushing to the queue)
silently loses every subsequent wake. Iteration (2) violated this and
the symptom only appeared when the producer churned the lock fast
enough to pop the waiter out of the queue while it was still ready to
park again.

The bulk-prefetch did not introduce the bug — it just made wm fast
enough to expose the race that had been latent since iteration (2)
landed.

## The fix shape (final)

### Primitive layer (`kernel/src/thread/{thread,scheduler}.rs`)
- `Thread.wake_pending: AtomicBool` token.
- Wakers call `signal_wake()` before probing state.
- `transition_park` / `transition_park_while` / `transition_sleep` call
  `consume_wake_pending()` after CAS to Parked/Sleeping; revert to
  Running if the token was set.
- `wake_thread_slow` and `wake_thread_from_irq` collapsed to one
  `do_wake`. No retry loop, no `MAX_RETRIES`, no spin.
- `thread_park_while` does ONE transition then returns. Spurious returns
  are part of the contract.
- New transition `Sleeping -> Running` for sleep abort.

### Wait-queue layer (`kernel/src/thread/waitqueue.rs`)
- `wait_internal` is documented as "exactly one push + one park + one
  remove per call." Callers loop externally and re-enter `wait_until`
  on each iteration to re-push the tid.

### Direct callers
- `sys_waitpid` (`kernel/src/syscalls/mod.rs`) wrapped in
  `while !has_exited(target) { thread_park_while(...) }`. The only
  bare caller; everything else (driver kthreads, sys_poll_events,
  Subscriber::recv, BlockingMutex::lock via WaitQueue, etc.) already
  loops.

### Debug visibility (`d08c38b`)
- `Thread.last_syscall` (relaxed store at top of syscall_handler) plus
  `tools/debug/dump_threads.gdb` `SYSCALL` and `WP` columns. Cuts
  diagnosis time for any future park/wake hang from "GDB walk
  BTreeMap and decode by hand" to "read the dump."

## Reasoning rules going forward (also in CLAUDE.md)

- Producers MUST `signal_wake` (or call a `wake_thread*` wrapper that
  does) BEFORE probing thread state. Establishes happens-before with
  the parker's `consume_wake_pending`.
- Parkers (the three `transition_*` functions) MUST `consume_wake_pending`
  AFTER CAS to Parked/Sleeping; revert to Running if set.
- `thread_park_while` MAY RETURN SPURIOUSLY. Callers MUST loop on the
  actual condition. Pattern: `while !done() { thread_park_while(|| !done()); }`.
- Wait-queue invariant: each `wait_until` call = one push + one park +
  one remove. Re-parking without re-pushing breaks this.

## If a similar hang reappears

Run `tools/debug/dump_threads.gdb` against the running QEMU.

- `WP=1` on a Parked thread → primitive bug (token set but parker did
  not consume). Should never happen with the current implementation.
- `WP=0` on a Parked thread, lock target free, wait queue empty →
  iteration (2)-style bug. Look for a `thread_park_while` caller that
  loops internally instead of letting the caller loop.
- `WP=0` on a Parked thread, lock target free, wait queue NON-EMPTY but
  thread not in queue → producer didn't call `wake_thread` after
  releasing the lock, OR the wake was sent to a handle pointing at a
  different Thread. (Since Foundation #1 landed 2026-04-16, sync
  primitives store `Weak<Thread>` — the "wake wrong thread after TID
  recycle" class of bug is closed at the API level. A dangling Weak
  makes `wake_thread` a silent no-op; never a misdirected wake.)
- `SYSCALL=FUTEX_WAIT|WINDOW_POLL|READ|...` tells you exactly which
  syscall the thread is parked in without a backtrace walk.

For lock-state inspection (e.g. is TTY_BUFFER held?), see
`/tmp/dump_tty.gdb` recipe pattern: parse the BlockingMutex's `locked`
atomic and the embedded WaitQueue's `front`/`back`/buffer. Empty deque
is `front == back && !full`.

## Saved artifacts (gitignored, in logs/) — kept for posterity

- logs/2026-04-12-boot-hang-missed-wakeup.log
- logs/2026-04-12-boot-hang-gdb-dump.txt
- logs/2026-04-12-boot-hang-thread-states.txt

These are from the original hang before the fix. Useful as a reference
for what a missed-wakeup hang looks like.
