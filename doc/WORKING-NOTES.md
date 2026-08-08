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
final count is not a race. Run it as `make test AUDIODEV=none` from a bare SSH
login: the default `pipewire` backend has no session bus to talk to there, and
QEMU refuses to start rather than falling back.

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

## Fixed: concurrent mmap handed the same address to several threads

Corrupted memory in any multi-threaded program. Fixed by making the claim atomic;
kept here because the symptom sent two separate investigations into the allocator.

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

`VmaSet::reserve` now runs the first fit and inserts the VMA under the one
acquisition the caller holds, and `find_free_address` is private, because an
address it returns is only free while the lock is held. `syscalls::memory::
claim_range` is the single entry point; there were **four** call sites, not one:

- anonymous `mmap`
- file-backed `mmap`
- `MAP_PHYSICAL` `mmap`
- `sys_shm_map`
- the 2 MiB thread stack in `sys_clone` — the worst of them, since every
  `std::thread::spawn` goes through it, so two concurrent spawns could share a
  stack

The two paths that can fail after claiming (physical `mmap`, `shm_map`) release
the range on the way out. Widening the guard instead was the alternative, and was
rejected: `vmas` is a `PreemptSpinlock`, so holding it across the page-table work
would turn every mapping into one non-preemptible span, and anything added to
that span that can park would then be a bug rather than merely slow.

Verified over ten `threadtest hammer` runs (eight threads each) across two
builds: no address appears twice within one address space, no faults, no panics.
`mmaptest` (10/10), `threadtest` and `forktest` pass, and the in-kernel suite is
47/47.

Mind how you check for this. Duplicates have to be counted **per address space**,
which means segmenting the log by process and keeping only that process's own
threads. Two naive versions of the check both cried wolf on me: separate runs of
a program are separate address spaces, and `mmaptest` execs two copies of `echo`
that legitimately map at the same address.

## Fixed: a syscall could run with a kthread as the per-CPU current thread

`bin/threadtest` panicked the kernel once in roughly eight runs with

```
KERNEL PANIC: current_thread_info: no UserThreadInfo for tid 3
  src/thread/scheduler.rs:1162
```

on `cpu-2`, while a kernel thread was current. `tid 3` is a kthread, and kthreads
have no `UserThreadInfo`, so the lookup failed. Every caller of
`current_thread_info()` lives in `kernel/src/syscalls/`, so a syscall handler was
running while that CPU's current thread was a kthread.

The receiver was the bug. `current_thread_info` was a method on `Scheduler`, and
it answered from `self.current` — the field of **one CPU's** scheduler. Callers
wrote

```rust
let sched = sched();                     // the CPU we are on *now*
let info = sched.current_thread_info();  // ...answered by that same CPU, later
```

and a syscall runs with interrupts enabled (the entry stub does `sti`), so the
caller can be preempted between those two lines and resume elsewhere. The
`&'static Scheduler` then names the CPU it has left, whose `current` has moved on
to another thread — a kthread, in the panic above.

Which thread is current is a property of the CPU executing right now, so it is no
longer reachable through a `&Scheduler` at all. `current_thread`,
`current_thread_id`, `current_thread_weak` and `current_thread_info` are free
functions that read the per-CPU slot with interrupts off, which makes the read
atomic against migration; the `Arc` they return stays valid however the thread
moves afterwards. `Scheduler::current` survives as the private `running_tid`, for
the scheduler internals that legitimately ask "what is *this* CPU running" from a
context that cannot migrate.

**The rule to keep: `&Scheduler` never means "me".** It means one specific CPU's
run queue. Anything phrased as "the current thread" belongs to the free functions.

`thread_exit` had the same defect with worse consequences: it cleared
`self.current` and decremented `self.thread_count`, so a migration mid-call left
the *departed* CPU believing it was idle while it ran someone else. It is a free
function now and resolves its scheduler inside the interrupt-off window.
`thread_yield`, `thread_park`, `thread_park_while` and `thread_sleep` moved too;
they never touched `self`, and leaving them as methods invited the same mistake.

Two latent bugs fell out with it. `lock_order::enter` compared a per-CPU
`current_thread()` against a scheduler-derived `current_thread_id()` and would
have fired its single-owner assert on any migration between the two; both sides
now read the same source. The `window-lock-debug` reader table recorded the tid
of whichever CPU the guard was taken on, which is exactly the wrong tid for the
instrumentation whose job is naming a stuck reader.

Why it surfaced when it did: no program used `std::thread` before, so userspace
never had several runnable threads competing across four CPUs. `threadtest`
exists to keep exercising that.

---

## Fixed: a CPU stopped answering TLB shootdown IPIs, then double faulted

Reproducible in about a minute, which the `current_thread_info` panic never was.
Drive `threadtest`, `threadtest hammer` and `threadtest nojoin` in a loop through
`scripts/edos-vm` on a 4-core boot. Around t=52s the log turns into nothing but

```
<cpu-2:bin/edos-wm:u:21> tlb_shootdown: timeout waiting for CPUs (mask=0x1), forcing clear
```

repeating (314 times in the observed run), and the desktop stops responding to
input while the taskbar clock keeps redrawing. `mask=0x1` is CPU0, and CPU0 never
acknowledges again. Register dump at that point:

| CPU | RIP | state |
|---|---|---|
| 0 | `interrupts::idt::double_fault_handler` | halted |
| 1-3 | `Scheduler::run_idle` | halted |

So CPU0 wedged first, kept missing shootdown IPIs until it double faulted, and
the rest of the machine went idle behind it.

A second run wedged with a different tail, and that one named the cause: three
CPUs spinning in `IrqSpinlock::lock` on the serial port with interrupts off, and
the fourth spinning in `tlb_shootdown` waiting for their acknowledgement.

This is **not** related to the identity fix above: it reproduces identically on
the commit before it (`f51ab70`), and slightly sooner (t=52s vs t=76s, 6 vs 10
completed `threadtest` runs).

### Fixed: `IrqSpinlock` waited with interrupts disabled

`IrqSpinlock::lock` disabled interrupts and *then* spun for the lock, so a CPU
waiting on a contended one answered no IPIs for the whole wait — including TLB
shootdowns. Interrupts only need to be off while the lock is *held*, which is
what keeps an IRQ handler from deadlocking against the holder; taking an IRQ
while still waiting is harmless, because the waiter does not hold it yet. It now
disables, tries, and re-enables around a read-only spin on the contended line.

The serial lock is what made this bite. Every thread exit logs a line, every
UART byte is a VM exit under KVM, and `threadtest` spawns some forty threads a
run, so under the loop above the serial lock is saturated and CPUs sit in that
IF-off wait for far longer than the shootdown's 10M-iteration timeout.

Effect on the reproducer: **916 shootdown timeouts became 0**, and the machine
survives the full loop where it previously stopped logging entirely at t=52s.

### Root cause: an idle CPU squats on a thread's kernel stack

`run_idle` holds `context` — a pointer to the interrupt frame — in a local
across `enable()` and `enable_and_hlt()`. On the timer-preemption path that
local and that frame both live on the **outgoing thread's kernel stack**,
because `timer_interrupt_handler` never pivots RSP. The voluntary path does the
opposite, and the comment on the scheduler-stack allocation in `init` says why:
it pivots "so the outgoing thread's kernel stack is completely free before any
waker can resume it".

By the time `pick_and_run` reaches `run_idle`, `maybe_preempt` has already run
`save_current_thread` (setting `context_saved = true`) and enqueued the thread,
so any other CPU may steal it and resume it *on that same kernel stack* while
this CPU is still idling on it. Two CPUs then write one stack, and the squatting
lasts as long as the CPU stays idle.

Caught with `--features trace` on a 10-core boot, first iteration of the loop:

```
cpu 0:  [36] Save   cpu=0 tid=46 rip=0x412cd9
        [37] Switch cpu=0 46->50
cpu 9:  [13] Steal  0->9 tid=46
        [14] Switch cpu=9 0->46 rip=0x412cd9     <- from_tid 0: CPU 9 was idle
```

CPU 9 panicked in that switch with `cw: Low context address 0x1` — its
`context` local had been overwritten while it idled. The same mechanism explains
the double faults and the impossible interrupt frames seen earlier, where
`instruction_pointer` held a plausible RFLAGS value (`0x286`) and `code_segment`
an index of 6400 against a seven-entry GDT.

### Fixed: leave the thread's stack before publishing the thread

Two paths kept using a kernel stack after handing the thread to somebody else.
Both now pivot to the per-CPU scheduler stack first, which is the discipline
`save_transition_switch` already followed and documented.

**`thread_exit` is the one this workload hammered.** It called
`reaper_enqueue(t)` and *then* `switch_away()`, which does `sub rsp, 160` and
calls into Rust — on the dying thread's kernel stack, the stack `Thread::free`
unmaps. The reaper runs on another CPU, so it may pull that stack out from under
the exiting thread at any point after the enqueue. `threadtest` exits roughly
forty threads per run, which is why it reproduced there and nowhere else.
`thread_exit` now only marks the thread `Dying`; `switch_away` pivots, and
`reap_and_schedule` posts to the reaper and picks the next thread from the
scheduler stack.

**The timer tick had the same shape.** `context_switch_to` writes the incoming
thread's frame into a frame sitting on the *outgoing* thread's stack, after that
thread has been enqueued and can already be running elsewhere. `on_tick` is
split into `tick_prepare` (thread stack; saves the outgoing context, returns the
stack to pivot to) and `tick_finish` (scheduler stack; enqueues and picks), with
the naked handler copying the 160-byte frame between them. `CpuContext` gained a
const assert on that 160, since three trampolines hard-code it.

Verified on a **10-core** boot, the configuration that previously died on the
first iteration: 25 iterations of the `threadtest` / `threadtest hammer` loop, 50
clean completions, 697 threads spawned and reaped, no panic, no double fault, no
shootdown timeout, no garbage in the log, every CPU idle afterwards. 47/47
in-kernel tests.

**A warning about reading the evidence here.** An intermediate build had only the
tick pivot, and it failed with the log prefix itself garbled
(`<cpu-633166472:kernel>`, uptime near `u64::MAX`). That looked like the pivot
had broken the GS-based per-CPU pointer. It had not: `_serial_print` formats on a
thread's kernel stack, so the still-unfixed exit path was corrupting the logging
path's own locals. Corrupted output names where corruption *landed*, not what
caused it — the same trap as the serial log ending mid-line in the wedges above.

### Fixed: the shootdown timeout acknowledged flushes that never happened

Separate from the stack bug, and wrong regardless of how often it fired.
`tlb_shootdown`'s timeout force-cleared `pending_mask` and returned, on the
reasoning that "the lagging CPUs will flush redundantly when they eventually
process the IPI, which is safe". It is not safe. Returning tells the caller that
no CPU holds the old translation, and the caller is entitled to free or reuse
the page on the strength of that; a CPU that never acknowledged is still reading
through the stale entry. The escape hatch traded a stall for silent corruption,
and the 314-timeout run above was doing exactly that, 314 times.

Three things were wrong and all three are fixed:

- **Giving up at all.** The wait now re-sends the IPI to the CPUs still
  outstanding and, if `ACK_ATTEMPTS` rounds pass with no acknowledgement,
  panics naming the mask and range. A wedged CPU is a bug worth stopping for;
  continuing is not a recovery, it is corruption with the evidence discarded.
- **Acknowledgements that credit the wrong round.** `pending_mask` was reused
  across rounds with nothing distinguishing them, so a late handler from a
  timed-out round could clear a bit for the round in flight — reporting a flush
  it never performed. A `generation` counter is bumped per round; the handler
  captures it before flushing and only acknowledges if it still matches.
  Skipping is safe: a round still waiting on that CPU has an IPI latched for it.
- **The initiator could be descheduled holding `active`.** Every other CPU
  wanting a shootdown spins on that flag, so the round now runs with preemption
  suppressed, per the rule in `thread/preempt.rs`.

Re-sending is cheap insurance rather than the main point: an IPI to a CPU with
interrupts off is latched and will fire, so a re-send only helps if one was
genuinely lost.

### Fixed: `pick_sched` sampled `thread_count` twice

Not part of the stack or shootdown bugs; found by soaking for them. `pick_sched`
made one pass to find the minimum `thread_count` and a second to find a
scheduler matching it. Other CPUs spawn and exit throughout, so every count can
rise above the sampled minimum in between, the second pass matches nothing, and
it reaches `unreachable!()`. It now takes one pass keeping the best sample,
starting at the rotation offset so the round-robin tie-break is unchanged.

Worth noting how it turned up: a soak that mixed `mmaptest` into the
`threadtest` loop, because `mmaptest` spawns a child of its own and roughly
doubled the spawn rate. Varying the workload found a bug that repeating the same
one never would.

---

## Audit, and the logging that came out of it

`doc/AUDIT.md` is a read-only pass over the whole tree: correctness, perf,
missing syscalls, smells, plus a list of things that looked like findings and
were checked and discarded. `ideas.txt` carries the prioritised follow-up.

One item is already fixed, because it was on every hot path. The kernel logged a
line per mmap, munmap, spawn, ELF load and thread exit. Each costs a `String`
allocation on the calling thread, and the drain side writes to the UART a byte
at a time under a global lock — one VM exit per byte under KVM. That is the same
serial lock whose saturation starved TLB shootdowns before `IrqSpinlock` stopped
waiting with interrupts off, so this was not a cosmetic cost.

`log_debug!` reads a relaxed atomic before formatting, so a disabled site costs
one load and no allocation. It is off unless the kernel command line carries
`loglevel=debug`, which makes it a dial rather than a rebuild. Failure paths
stayed on `log!`. Six `threadtest`+`hammer` iterations went from dozens of lines
each to **zero**; one `threadtest` with `loglevel=debug` still emits 37.

Two traps worth knowing if you touch this:

- **`ParsedCmdline::parse_str` allocates**, so reading the log level has to
  happen *after* `init()` brings the frame allocator up. Putting it before
  panics at `frame_allocator.rs:24` before serial is useful.
- **The serial log is no longer a way to count work.** Greps like
  `bin/threadtest:u:.* exit: code=` return nothing by default now. Use the
  terminal output, or boot with `loglevel=debug`.

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
