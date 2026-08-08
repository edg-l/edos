# Window registry stuck reader: whole-GUI deadlock

**Status:** fixed. The scheduler could pass over a runnable thread forever, which
makes any spin lock shared across priorities a deadlock rather than a slowdown.
Two scheduler defects are fixed and covered by a regression test
(`starvation-victim` in `thread/sched_test.rs`), which without them shows a
default-priority thread making *zero* progress.

The hang itself was observed once, after ~630s of continuous synthetic clicks and
keystrokes through `scripts/edos-vm`, and has not been re-observed with the
instrumentation armed. Attributing it to starvation rests on the mechanism being
proven to exist and to produce exactly the recorded signature, not on having
caught it in the act. What would settle it is naming the holder with
`--features window-lock-debug` and reading its `State`.

A 3000-round instrumented soak afterwards (`scripts/window-lock-soak`, ~45
minutes, 207k registry acquisitions) did not reproduce it, and the reader table
stayed empty throughout. That run included the `sys_window_list` fix below,
which shrinks the exposure window by orders of magnitude without closing it.

---

## Symptoms

The desktop freezes completely and never recovers:

- Taskbar clock stops advancing.
- Pointer frozen; further mouse motion and keystrokes have no effect.
- Serial log goes permanently silent (last line was a routine `edos-wm` mmap).
- One host vCPU thread pinned at 99.9%, the other three idle.
- The guest is *not* panicked. `query-status` still reports `running`.

Every vCPU is spinning in `spin::RwLock<WindowRegistry>::write`:

| CPU | RIP | Site |
|---|---|---|
| 0, 1, 3 | `0xffffffff800227cd` | `sys_window_set` (`syscalls/window.rs:128`) |
| 0 (later sample) | `0xffffffff80022feb` | `sys_window_list` (`syscalls/window.rs:366`) |
| 2 | `0xffffffff80126d6b` | `handle_mouse_event` (`window/input.rs:332`) |

---

## Root cause

Reading the lock word out of the hung guest is decisive:

```
x /1gx 0xffffffff801a5c40    # WINDOW_REGISTRY
ffffffff801a5c40: 0x0000000000000004
```

`spin` encodes `READER = 1 << 2`, `UPGRADED = 1 << 1`, `WRITER = 1`. The value
`0x4` means **one outstanding reader and no writer**. This is not a writer
deadlock. A single read guard leaked, and since a writer can never acquire while
any reader is outstanding, every subsequent `write()` spins forever.

`WINDOW_EVENTS` at `0xffffffff801a8f40` reads `0x0`, so it is not involved.

### What the evidence rules out

The first hypothesis was a guard leaked by a killed thread: EDOS is `no_std` with
no unwinding, so a thread killed while holding a guard never runs
`RwLockReadGuard::drop`. **The serial log disproves this.** Across the whole run
there are no panics, no page faults, and no non-zero exits; all ten recorded
exits are `code=0`, and the last one is at t=410s, 219 seconds before the hang at
t=629s. Nothing died.

### The holder was starved, not parked

The reader count of 1 is *legitimate*: a thread acquired the read guard and then
stopped making progress without releasing it. The register dump shows only that
it was not **running** on any CPU. `Ready` fits that evidence as well as
`Parked` does, and needs no lost wake to explain a permanent hang.

`Ready` is the better fit. Preemption is involuntary, so any thread can be
preempted mid-guard, and two scheduler defects then made the wait unbounded:

1. **The timeslice was armed but never enforced.** `context_switch_to` computes
   `slice_deadline` and arms the APIC timer to it, but `maybe_preempt` returns
   early unless `NEED_RESCHED` is set, and nothing set it on expiry;
   `slice_deadline` was read only by procfs. Preemption therefore happened only
   when a thread was enqueued on that CPU or a wake nudged it.
2. **Anti-starvation applied only to wake-boosted threads.** `pop_next` reached
   `pop_lower_than` only when `rq_boosted` was set, which happens for
   `WakePriority::Interrupt` wakes alone. A thread at a high *base* priority
   reset the streak on every pick, so a lower level was never serviced.

The window-input kthread runs at priority 10 (`window/input.rs`) and user
threads default to 7. So: a user thread is preempted while holding the read
guard; the mouse IRQ enqueues the input kthread on that same CPU; the kthread
reaches `WINDOW_REGISTRY.write()` in `handle_mouse_event` and spins; `pop_next`
prefers priority 10 forever and the holder is never picked again. Every other
CPU then piles onto the write. That reproduces every recorded fact: one reader
and no writer, nothing killed, the holder absent from all four register dumps,
CPU 2 sitting in `handle_mouse_event`, and no recovery.

Nothing rescues it. Work-stealing runs from `run_idle` and no CPU was idle;
`try_rebalance` keys off `thread_count`, which counts assigned threads including
parked ones, and needs an imbalance of two.

### The fix

Both defects are fixed in the scheduler, so the bound applies to every lock in
the kernel rather than to this one:

- `Scheduler::expire_timeslice` marks `NEED_RESCHED` once `slice_deadline` has
  elapsed, giving the CPU back on a schedule instead of only on external events.
- `RunQueue::pop_next` counts every pick, not just boosted ones, and services
  the highest non-empty lower level every `STARVE_STREAK_LIMIT` picks.

Separately, `WINDOW_REGISTRY` and `WINDOW_EVENTS` are now `IrqRwLock`, holding
their guards with interrupts disabled so the sections cannot be preempted at
all. With the scheduler fixed that is no longer required for correctness; it
bounds how long other CPUs spin, and it is the standard rule for a spin lock
shared across priorities.

`starvation-victim` in `thread/sched_test.rs` is the regression test: one
CPU-bound spinner per CPU above `DEFAULT_PRIORITY`, and a default-priority
thread whose progress the spinners sample across the fully saturated window.
With either fix disabled the victim advances by exactly 0.

### Confirmed defect, fixed

Independent of the above, `sys_window_list` held the read guard across
`try_copy_to_user` for every visible window. A user copy can demand-fault, and a
ring-0 demand fault runs with interrupts enabled and can park on a page fill.
Holding a spin lock across a park makes every other CPU spin for the duration of
disk I/O, which matches the observed "all four CPUs spinning" shape exactly.

Fixed by snapshotting the entries into an owned `Vec` under the guard, dropping
the guard, then copying out. This is a real defect on its own merits; whether it
is *the* cause of this hang is unproven.

---

## Reasoning rules going forward

- **Never hold a spin lock across a user-memory access.** A user copy can
  demand-fault, and the ring-0 branch of the page-fault handler services demand
  faults *before* checking the uaccess fixup, on a path documented as blocking.
  Snapshot into a kernel-owned buffer, drop the guard, then copy out. This is the
  rule the FS layer already follows for `inode.lock` and disk I/O, applied to
  userspace copies.
- **A spin lock is only bounded if its holder is guaranteed to run.** Every other
  CPU busy-waits behind the holder, so anything that can stop the holder
  indefinitely — parking, or a scheduler that will not pick it — is a deadlock,
  not a slowdown.
- **"Not running" is not "parked".** A register dump shows which threads are
  Running; it says nothing about whether the missing one is `Parked` or `Ready`.
  Distinguishing them changes the diagnosis completely, so read the state rather
  than inferring it.
- **A guard held across a killable operation would also leak**, because there is
  no unwinding: the reaper frees the stack without running destructors. That did
  not happen here, but the hazard is real and worth avoiding for the same reason.
- **A `spin::RwLock` reader leak is strictly worse than a mutex leak**, because
  it is invisible until the first writer arrives, and then it takes down every
  CPU at once rather than one caller.
- **Unranked does not mean harmless.** `WINDOW_REGISTRY` was excluded from the
  rank table (`doc/invariants/lock-order.md`) on the grounds that window
  registries are leaf locks outside the fs/mm hot paths. That is true and it did
  not help: the failure was hold-duration and kill-safety, not acquisition order.

---

## Audits

Both audits came back clean, which is what pushed the diagnosis out of the
window code and into the scheduler.

- **Guard sites.** Every use of `WINDOW_REGISTRY` and `WINDOW_EVENTS` lives in
  `window/` and `syscalls/window.rs`: seven read sites, eight write sites.
  `sys_window_set` hoists its `try_read_user` above the write guard and then only
  assigns fields. `send_event` under a read guard is a lock-free `ArrayQueue`
  push. No remaining site touches user memory under a guard. Allocation does
  happen under a guard (`sys_window_list`, `create_window`), but the kernel
  allocator is `IrqSpinlock` plus spin loops throughout and never parks.
- **Park/wake compliance.** One `thread_park_while` exists in the whole window
  path, `window/input.rs:286`, correctly wrapped in a loop that re-checks via
  `try_recv`.

Ranking `WINDOW_REGISTRY` would not have caught this; the failure is hold
duration, not acquisition order.

---

## If this reappears

1. `scripts/edos-vm qmp human-monitor-command '{"command-line":"info registers -a"}'`
   and symbolize each RIP with `addr2line -e kernel/target/x86_64-unknown-none/debug/edos-kernel -f -C -i`.
2. If the RIPs land in `spin::rwlock`, read the lock word for the contended lock.
   `nm -C` the kernel for the symbol, then `x /1gx <addr>`.
3. Decode: `& 1` is a live writer, `>> 2` is the reader count. A non-zero reader
   count with no writer, and no forward progress, means a stuck reader rather
   than a lock cycle: look for a holder that parked, was starved, or died with
   the guard live. Name it with `--features window-lock-debug` and then read that
   thread's `State` — `Ready` and `Parked` have completely different causes.
4. Check the serial log for kills and non-zero exits before assuming a kill leak.
   Their absence means the holder is parked, not dead.

---

## Saved artifacts

`logs/2026-08-08-window-registry-hang/` (gitignored): serial log, all four
register dumps, screenshot at the hang, and working notes.
