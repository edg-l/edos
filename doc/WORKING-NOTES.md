# Working notes, session of 2026-08-08

State of the tree, what changed, and what is still open. Written for whoever
picks this up next, which will usually be an agent with no memory of the
session.

---

## The big change: the OS is now driven by an agent, not by hand

`make run` needs a local display, which is useless over SSH. `scripts/edos-vm`
boots the same ISO headless and exposes two channels: VNC for a human, and QMP
for scripts. QMP gives screenshots as PNG, synthetic keystrokes, and pointer
events, so the whole desktop can be driven and observed from outside the guest.

Read [`vm-control.md`](vm-control.md) before touching it. Three guest properties
will otherwise waste an hour: the keymap is Spanish ISO, the mouse is HID boot
protocol so absolute pointing is silently ignored, and the window manager
focuses on click so keystrokes go nowhere until you click into a window.

This immediately paid for itself: ten minutes of scripted input found a
whole-GUI deadlock that manual use had never hit, because nobody clicks that
fast for that long.

---

## Fixed and verified on hardware

- **User virtual address space is reused.** `find_free_address` was a monotonic
  bump allocator that never reclaimed anything, burning ~940 MB of address space
  per 9.2s on an idle desktop against 2.4 MiB of live mappings. Now a first fit
  over the VMA tree. Stride fell to 8-10 MB and successive mmap/munmap cycles
  return the same address.
- **`sys_window_list` no longer holds a spin guard across a user copy.** A user
  copy can demand-fault and park, and parking with a spin guard live stops every
  other CPU. It now snapshots under the guard and copies outside it.
- **Filesystem errors keep their errno.** `sys_list_dir` and `sys_open`
  flattened everything to EINVAL despite a correct `From<FsError> for Errno`
  existing. Missing paths report ENOENT now.
- **`make filesystem` creates the directories it claims to.** It used brace
  expansion, and make runs recipes under dash, so it silently created one
  directory literally named `{bin,dev,home,...}` and `/var` never existed.
- **`OpenOptions` opens files for writing.** `read`, `write`, `truncate` and
  `create_new` were no-op stubs in the std fork, so every file was read-only as
  far as the kernel was concerned. This is why `mmap(MAP_SHARED, PROT_WRITE)`
  failed. Fixed in the fork as commit `b7af81795f6`, **committed locally in
  `~/dev/rust` but not pushed**, so it exists on this machine only.
- `sha256sum` and `file`, two Phase 3 userspace programs.

`mmaptest` went from failing at test 1 to all 10 passing on both `/var` and
`/tmp`.

**`VfsInode::drop` no longer panics the kernel on the reaper.** The drop-contract
guard asserted that the drop never *runs* on the reaper or evict kthread, but the
contract is that it never *blocks*, and the whole point of posting to the evict
kthread is to make the reaper path safe. The reaper frees a dead thread's FDs and
VMAs, so it routinely releases the last reference to an orphaned inode:
`mmaptest`'s unlink-while-mapped test panicked the kernel on trunk. The guard now
sits on the one blocking path, the queue-full fallback in `post_evict`, where the
reaper gives the eviction up (counted as `dropped_count` in `/proc/evict_stats`,
reclaimed by `efs-fsck`) instead of stalling teardown behind disk I/O. `mmaptest`
now passes 10/10 on both `/var` and `/tmp` with no panic.

**`make test` is green for the first time, and covers more**, 47/47 (was 30).
Added: the preemption counter's nesting and balance, `BlockingMutex` mutual
exclusion under contention, `BlockingRwLock` reader sharing plus writer
exclusion, and `WaitQueue::wake_all` releasing every waiter. Each was checked
against a deliberately broken build first — the mutex test reports 500 of 2000
increments when the guard is dropped across the read-modify-write, and the
waitqueue test strands three waiters when `wake_all` is swapped for `wake_one`.
Both handshakes are counter-based rather than timed: an earlier version waited
only for the queue to become non-empty and flaked about once in twenty. It was red on trunk: the
`abort-race` test called `thread_park_while` bare and treated any return as a
completed round, which is the exact contract violation
`bugs/2026-04-13-sched-park-wake-missed-wakeup.md` warns about. It now loops on
its condition, and the waker counts a round before releasing the parker so the
final count is not a race. Note that `make test` itself still fails to launch
over SSH because it passes `-audiodev pipewire`; substitute `-audiodev none`.

---

## The GUI deadlock was a scheduler bug, and is fixed

**A window-registry reader wedged the whole GUI**, with all four CPUs spinning
on `WINDOW_REGISTRY.write()`. Full writeup in
[`bugs/2026-08-08-window-registry-stuck-reader.md`](bugs/2026-08-08-window-registry-stuck-reader.md).

The holder was never parked. It was **`Ready` and starved**: the register dump
only proves it was not *running*, and the scheduler could pass over a runnable
thread forever. Two defects made the wait unbounded, both now fixed:

- **The timeslice was armed but never enforced.** `context_switch_to` set
  `slice_deadline` and armed the timer to it, but `maybe_preempt` bails unless
  `NEED_RESCHED` is set and nothing set it on expiry; `slice_deadline` was read
  only by procfs. A thread was preempted only when another became runnable on
  its CPU. `Scheduler::expire_timeslice` now marks it.
- **Anti-starvation only covered wake-boosted threads.** `pop_next` reached
  `pop_lower_than` only when `rq_boosted` was set, which happens for
  `WakePriority::Interrupt` wakes alone, so a high *base* priority thread
  starved everything below it. It now counts every pick and services the highest
  non-empty lower level every `STARVE_STREAK_LIMIT`.

The window-input kthread runs at priority 10 and user threads at 7, so a
preempted guard holder behind that kthread was never picked again. The same
hazard applied to every spin lock shared across priorities, including `VFS`.

`starvation-victim` in `thread/sched_test.rs` is the regression test: one
CPU-bound spinner per CPU above `DEFAULT_PRIORITY` plus a default-priority
thread whose progress the spinners sample across the saturated window. With
either fix disabled the victim advances by exactly 0; with both it advances by
~800k.

The reader instrumentation is still there and still useful, since it names the
holder rather than its state:

```bash
make edos-x86_64.iso CARGO_FLAGS="--features window-lock-debug"
scripts/edos-vm start
scripts/window-lock-soak 3000
```

Slots decode as `(tid << 8) | site`. `WINDOW_REGISTRY_READER_ACQUIRES` is the
positive control: live slots last microseconds, so an empty table only means
something if that counter is moving. It reads about 259/sec on an idle desktop.
Having named a holder, read its `State`: `Ready` and `Parked` have completely
different causes.

On top of the scheduler fix, spin locks shared between threads now suppress
preemption for the guard's lifetime (`thread/preempt.rs`): a per-CPU counter
that `maybe_preempt` honours, plus `PreemptSpinlock`/`PreemptRwLock`. Converted:
`WINDOW_REGISTRY`, `WINDOW_EVENTS`, `VFS` (rank 10), `UserThread.vmas` (70),
`memory_manager` (80), `SHARED_MEMORY_REGISTRY`, and the thread registries.

Suppressing preemption rather than interrupts is deliberate: `memory_manager`
walks page tables and `vmas` walks the VMA tree, so disabling interrupts across
them would trade a scheduling problem for a much worse interrupt-latency one.
`thread_park*`, `thread_sleep` and `thread_yield` debug-assert that preemption
is enabled, which doubles as an automated audit for "spin lock held across a
park" — it stayed silent through boot, the stress tests and the FS paths.

Still bare, deliberately: the scheduler's own `rq`/`sleepers`/`SCHEDULERS` and
`WaitQueue.inner` (wrapping them would recurse into the counter), and the
IRQ-reachable locks that correctly use `IrqSpinlock`.

---

## Cross-repo, deliberately not done

Both need a decision rather than a drive-by, because they leave this repo.

1. **The userspace allocator fragments without bound.** This is what looked like
   an `edos-wm` heap leak: 64 KiB every 9.2s forever on an idle desktop. It is
   `edos_rt`'s `PoolAllocator`, which never coalesces adjacent free blocks, drops
   blocks smaller than `FreeBlock`, and loses the tail of exactly-fitting blocks.
   Growth tracks allocation *rate*, not retention, which is why the period is so
   exact. Every long-running program is affected. Fixing it means publishing to
   crates.io, which is irreversible.
2. **The std fork fix above is committed but unpushed** (`b7af81795f6` on
   `edos_std_v2`). Push it, or the next person who rebuilds the toolchain from
   a fresh clone loses the fix and mmaptest regresses to failing at test 3.
   `edos_rt` still has no `RDONLY`/`WRONLY`/`RDWR`/`TRUNCATE` constants, so std
   spells the values out itself; moving them into `edos_rt` is cleaner and needs
   a release.

Also open, lower priority: `decode_error_kind` in the std fork maps only five
errnos, so everything else displays as "uncategorized error", and the AHCI
watchdog `restarting` gate, which is a latency issue rather than a lost I/O and
should land with runtime validation because it touches the storage submit path.

---

## Things that will bite you

- `make edos-x86_64.iso` re-invokes the kernel target **without** any
  `CARGO_FLAGS` you passed earlier, silently replacing an instrumented build
  with a plain one. Pass the flags to the ISO target itself.
- `cargo` does not notice that `std` changed. After rebuilding the toolchain,
  `cargo +edos clean` in `programs/` or you will keep linking the old one, and
  the build will cheerfully report success.
- `sg` is also the name of the `ast-grep` binary. Scripts that need the group
  tool must use `/usr/bin/sg`.
- **`alloctest` never exits, by design.** Its whole body is
  `loop { let v = vec![0u32; 256]; black_box(&v); drop(v); }`, an allocator
  soak with no termination condition. It is not a hang and not a bug. Anything
  that runs the stress binaries in sequence will sit there forever and silently
  buffer the rest of the input; run it last, or not at all.
- Symbol addresses move on every kernel rebuild, so resolve them from
  `kernel/kernel` at runtime rather than hard-coding them.
