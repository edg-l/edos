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

## Cross-repo state

The userspace allocator and the std fork both live outside this repo, and both
are now current. Two traps to know before you touch either again.

**The `edos_rt` clone can be behind crates.io.** 0.0.34 and 0.0.35 were
published from a tree that never landed in `github.com/edg-l/edos_rt`, so the
repo was two releases behind and a patch on top of it would have silently
reverted file-backed `mmap`, `msync` and the `OpenFlags` access-mode constants.
Diff the clone against the published crate before editing:

```bash
curl -sL -o /tmp/rt.crate https://crates.io/api/v1/crates/edos_rt/<max_version>/download
mkdir -p /tmp/rt && tar xzf /tmp/rt.crate -C /tmp/rt --strip-components=1
diff -ru /tmp/rt/src ~/dev/edos_rt/src
```

**The std fork's pin is the version that actually runs.** `library/std/Cargo.toml`
sat at `edos_rt = "0.0.26"` for ten releases while the crate moved on, and a
`0.0.z` requirement is exact, so none of that work reached any program. It is now
0.0.36. The full loop for an allocator or syscall-wrapper change is: patch
`edos_rt`, bump, `cargo publish`, bump the pin, `cargo +nightly update
--manifest-path library/Cargo.toml -p edos_rt`, `./x install` in `~/dev/rust`
(prefix `~/dev/edos-toolchain`, linked as the `edos` toolchain), then
`make programs`.

`PoolAllocator` fragmentation is fixed in 0.0.36: the free list is
address-ordered and coalescing, and the header records the whole reserved span
rather than the requested size. 0.0.37 then released idle chunks back to the
system, gave large allocations a header so alignment above a page is honoured,
and added a bounded cache of freed large mappings. `bench/allocstress` in the
`edos_rt` repo is the regression check; it compiles the allocator against a
shimmed `mmap` on the host and fails if the pool does not plateau, if freeing
everything does not hand the memory back, or if an over-aligned large request
comes back misaligned.

0.0.37 also carries the runtime fixes that came out of reading the rest of the
crate: the syscall wrappers are inlinable Rust-ABI functions instead of
`no_mangle extern "C"`, `thread_join` blocks in the kernel rather than polling
at 1 kHz, `getrandom` fills the whole buffer instead of returning a count std
discarded, `IoError` is the `Errno` itself so a caller can tell a missing path
from a full disk, and `Mutex` only enters the kernel when a waiter is actually
parked. `decode_error_kind` in the fork covers every `Errno` now, which was only
possible once the errno stopped being folded away below it.

0.0.38 followed, for two reasons that are worth separating from the bug below.
The allocator's own locks went back to a spin lock: its critical sections are a
few list operations long, so parking under them bought nothing and put a syscall
in the middle of a list walk, and the preempted-holder hazard that motivated the
change is bounded now that the kernel enforces the timeslice. The inline syscall
wrappers also stopped declaring the argument registers as merely read; a syscall
that parks resumes its caller through the scheduler rather than straight back out
of the entry stub, and `in(...)` promises the compiler those registers survive
that path too. They are `inout(...) => _`, which is what the out-of-line
`extern "C"` call implied before inlining.

Neither of those was the corruption. **Do not repeat the mistake of reading a
timing change as a fix**: the spin-lock build looked clean for several runs and
the futex build lost threads, which is what the difference in scheduling looks
like when the real fault is a narrow race elsewhere. The next section is the
actual cause.

Also open, lower priority: the AHCI watchdog `restarting` gate, which is a
latency issue rather than a lost I/O and should land with runtime validation
because it touches the storage submit path.

---

## Open bug: concurrent mmap hands the same address to several threads

This is the one to fix first. It corrupts memory in any multi-threaded program.

`bin/threadtest hammer` runs eight threads allocating hard. The serial log shows
three of them receiving the *same* mapping:

```
thread-75: mmap: lazy mapped at 0x143b000
thread-76: mmap: lazy mapped at 0x143b000
thread-73: mmap: lazy mapped at 0x144b000
thread-72: mmap: lazy mapped at 0x143b000
```

`sys_mmap` picks the address under one acquisition of the VMA lock
(`syscalls/memory.rs:115`, `vmas.find_free_address`) and inserts the `Vma` under a
separate, later one (`syscalls/memory.rs:212`). Two threads can therefore both run
the first fit, both see the range free, and both take it. The window is small; it
needs several threads calling `mmap` at once to hit.

The consequence is exactly the corruption that looked like an allocator bug: two
threads' `PoolAllocator` chunks alias the same pages, so one thread's free-list
links land in the other's blocks, and `alloc` then faults reading a link from an
address like `0x28`. Chasing it through the allocator wasted a lot of time, twice.

Worth knowing: `find_free_address` became a first fit over the VMA tree in the
same session that fixed the VA leak, and first fit **reuses** freed ranges, so a
stale pointer now lands in live memory instead of an unmapped hole. That makes
any aliasing far more damaging than it would have been under the old bump
allocator.

The fix has to make choosing and claiming a range atomic: either hold the VMA
lock from the first fit through the insert, or have the VMA set reserve the range
under one acquisition and hand back a placeholder the caller fills in. The
straight "widen the guard" version needs checking against the mapping work that
currently runs between the two points, which takes the mapper locks, so mind
`doc/invariants/lock-order.md`.

## Open bug: a syscall can run with a kthread as the per-CPU current thread

`bin/threadtest` panicked the kernel once in roughly eight runs with

```
KERNEL PANIC: current_thread_info: no UserThreadInfo for tid 3
  src/thread/scheduler.rs:1162
```

on `cpu-2`, while a kernel thread was current. `tid 3` is a kthread, and kthreads
have no `UserThreadInfo`, so the lookup fails. What makes it interesting is that
**every** caller of `current_thread_info()` is in `kernel/src/syscalls/`, so a
syscall handler was running while this CPU's current thread was a kthread. The
usual shape is

```rust
let sched = sched();                     // scheduler for whichever CPU we are on
let info = sched.current_thread_info();  // ...re-derived from that CPU
```

A syscall runs with interrupts enabled (the entry stub does `sti`), so it can be
preempted and resume on another CPU between those two lines, at which point the
identity is read from the wrong CPU. The likely real fix is to stop re-deriving
the caller: resolve `UserThreadInfo` once at syscall entry and pass it down,
rather than asking "who is running here" repeatedly.

Why it surfaced now: no program used `std::thread` before, so userspace never
had several runnable threads competing across four CPUs. `threadtest` exists to
keep exercising that. It is intermittent; six consecutive runs after the first
sighting were all clean, so reproducing it wants a loop, `run-big`, or the
`trace` feature (which dumps per-CPU trace buffers on panic).

Not caused by `edos_rt` moving `thread_join` onto the blocking `waitpid`: the
shell has always waited on children with `block = 1` through the same waiter
machinery, and the no-join variant (`threadtest nojoin`) also survives, so the
trigger is having many short-lived threads, not the join.

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
