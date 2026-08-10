# Working notes, sessions of 2026-08-08 to 2026-08-11

State of the tree, what changed, and what is still open. Written for whoever
picks this up next, which will usually be an agent with no memory of the
session.

---

## Start here for anything about storage performance

`programs/fsbench` measures the filesystem across idioms and depths: a memory
filesystem, a raw block device, and EFS. **Do not benchmark storage by hand or
write a one-off test — run it.** It also verifies what it wrote, and prints the
delta of every relevant `/proc` counter, which is what turns a number into a
diagnosis.

```bash
fsbench -l /var              # EFS: writes, reads, metadata, verify
fsbench -l raw /dev/sda      # the block layer and AHCI ceiling
fsbench -l rawwrite /dev/sdX # destructive; refused on a mounted device
fsbench -l /tmp              # memfs: the syscall and copy ceiling
```

`-l` mirrors the report to `/dev/klog`, which lands in `run_log.txt` on the
host — the guest terminal is far too short to hold a full run.

- [`fsbench.md`](fsbench.md) — how to run it, what each number means, and the
  record of what the 2026-08-09 round found and fixed.
- [`STORAGE-ROADMAP.md`](STORAGE-ROADMAP.md) — what is worth doing next, in
  order, with the evidence for each, **and a list of five experiments that
  measurement refuted.** Read that list before optimising: two of them sounded
  obviously right and made the system slower.

Two traps that round produced, both of which made a number mean the opposite of
what it said:

- A throughput figure is meaningless unless you know whether the work was
  deferred. A buffered `write` returns at page-cache speed; only the `fsync`
  rows and `sync()` measure the disk.
- Reading back in the same boot reads the page cache. Cold numbers need
  `fsbench write`, a reboot, then `fsbench read`, which is also what
  `scripts/fs-regression` does for durability.

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
  failed. Fixed in the fork as commit `b7af81795f6`, on `origin/edos_std_v2`.
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
`WINDOW_REGISTRY` (280), `WINDOW_EVENTS` (290), `VFS` (10), `UserThread.vmas`
(70), `memory_manager` (80), `SHARED_MEMORY_REGISTRY` (90), the input
`Broadcaster` (310), and the thread registries.

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

## Fixed: an unvalidated address reached a VMA insert

Audit item 1.1 was that the ELF loader builds a mapping out of
`base_addr + p_vaddr` without ever bounding it, and that `VmaSet` applies
`USER_VA_END` only to addresses it picks itself. The audit could not say how bad
that was without building a crafted ELF.

No crafted ELF was needed. `sys_mmap` reaches the same insert with a raw user
address, and validated nothing beyond `length != 0`:

```
mmap(addr=0x0000_9000_0000_0000, len=0x1000)   # non-canonical, from any program
  -> claim_range -> VirtAddr::new
  -> KERNEL PANIC: virtual address must be sign extended in bits 48 to 64
```

Reproduced on a pre-fix kernel and resolved through the backtrace to
`syscalls/memory.rs:234`. Every VMA a process holds becomes a `USER_ACCESSIBLE`
mapping, so the canonical-but-kernel-half case (`0xffff_8000_…`) was the worse
half of the same hole: it does not panic, it inserts.

The check belongs in `VmaSet::insert`, which now returns `Result` and rejects a
range that wraps or ends past `USER_VA_END`. Callers that hand back a range the
set already held — unmap rollback, fork's deep copy, the TLS region the kernel
derives from `USER_STACK_TOP` — call `insert_validated`, which debug-asserts
instead; they have no error to report and no untrusted input. The loader bounds
the segment with `checked_add` before constructing a single `VirtAddr`, and
rejects `p_filesz > p_memsz`, which would otherwise push the file-backed VMA
past the end that was checked.

Two neighbours fell out of the same read:

- **`find_free_address` had the same bug in its align-up.** `(length + 0xfff)`
  wraps for a length within a page of `u64::MAX`, so it returned a gap far
  shorter than requested. Now `checked_add`.
- **Address-space exhaustion was an `expect`.** It reports `VmaError::NoSpace`
  (ENOMEM) instead of panicking.

`mmaptest` test 11 is the regression test: five cases (non-canonical, kernel
half, straddling the top, wrapping length, unsatisfiable length), each of which
must come back as a failed `mmap`. 11/11 on both `/var` (EFS) and `/tmp`
(memfs), 47/47 in-kernel tests, forktest and threadtest clean.

---

## Fixed: CPU affinity was a field, not a rule

`cpu_affinity` had a setter and one enforcement point. `thread_can_run_here` was
stubbed to `true` with the real check commented out beneath it, and
`complete_wake` enqueued on the waker's CPU without consulting it, so a pin held
only until something woke the thread.

Affinity is a **placement** property in this kernel: `spawn_thread`,
`complete_wake` and work-stealing pick the CPU, and `pick_and_run` runs whatever
it pops without re-checking. That is the cheaper design and it is now the
documented one — `set_affinity_mask` says a mask set on a running thread applies
at its next placement, not immediately.

The trap worth knowing: **un-stubbing `thread_can_run_here` alone would have
lost threads.** `spawn_thread`'s `else` arm was a bare comment claiming the
thread "will be queued on its target cpu by that cpu's scheduler", and nothing
did. The stub returning `true` is the only reason that arm never ran. It routes
through `pick_sched_for` now, and a mask naming no registered CPU runs the
thread here rather than dropping it.

Two notes on the test, because the first version of it was worthless:

- **Yields do not test affinity.** They re-enqueue on the CPU the thread is
  already on, so a thread that reached the right CPU by luck stays there. The
  first `affinity-pinned` passed with `allows_cpu` hardcoded to `true`.
- **Wakes do.** `complete_wake` prefers the waker's CPU, and the waker is
  elsewhere most rounds, so the pin only survives a wake if the check is real.
  With `allows_cpu` reverted the test dies on round 0: "pinned to cpu 3, ran on
  cpu 2 after wake 0". Any future change here should be checked the same way —
  revert the predicate, confirm the test fails.

A third thing fell out of the same read: `pick_sched` called
`schedulers.iter().nth(idx)` per candidate (audit 2.4), now one `cycle().skip()`
pass.

47 → 49 in-kernel tests, all passing, desktop and mmaptest/threadtest clean
afterwards.

---

## The rest of the audit, and two things it got wrong

Shipped: `clock_gettime` off the RTC (sampled once at boot, pinned to the HPET,
nanoseconds since the epoch); path syscalls on a stack buffer via
`copy_user_path`; `pread`/`pwrite` and `getuid`/`getgid`; the five bare
`spin::Mutex` sites on `PreemptSpinlock`; an RFC 6298 retransmit timeout for
TCP; `fs/api.rs` returning `ProtocolMismatch` instead of panicking.

Two audit recommendations were **checked and rejected**, which is the part worth
remembering:

- **CLOEXEC has nothing to govern.** There is no `exec` in this kernel. `spawn`
  builds a fresh process and gives it exactly three descriptors; `fork` copies
  the table, which is what fork does. No `O_NONBLOCK` exists either, so
  `F_SETFL` would set nothing. The flag becomes real the day `exec` lands.
- **`setuid` without a permission model** is a privilege change that enforces
  nothing. `getuid`/`getgid` are in; the setter is not.

Two bugs fell out of writing the tests rather than out of the audit:

- **`sys_read` held the fd-table `BlockingMutex` with interrupts disabled.**
  `sys_write` and `sys_close` clone the Arc, enable interrupts, then lock;
  `sys_read` locked inside the `UserThreadInfo` `IrqSpinlock` scope. Eight
  threads doing positional reads through one shared descriptor tripped the
  contended-with-interrupts-off assert, and the spinning then starved a TLB
  shootdown into its timeout. The same shape as the `IrqSpinlock` bug from
  earlier in the session: *the assert fires on contention, so a rarely-contended
  wrong lock looks fine for months.*
- **TCP cannot connect at all**, and never could — a pre-session build fails
  identically. `doc/bugs/2026-08-08-tcp-connect-rsts-its-own-synack.md` has the
  packet capture and the instrumentation results. It stayed hidden because
  `http`/`wget` use `std::net::TcpStream`, which the std fork does not
  implement, so nothing had ever completed a connection.

The RFC 6298 work is therefore correct by inspection but **unverified end to
end**; it cannot be exercised until a connection can reach Established.

---

## execve exists now

`execve` (59) replaces a process image in place, with `fcntl` (72) and
`FD_CLOEXEC` alongside it. The shape that matters, because it is what makes the
operation safe:

1. **Copy argv/envp/path out of user memory first.** The address space holding
   those strings is about to be unmapped.
2. **Build the new image in a fresh address space while the old one is live.**
   A load failure then returns an error with the process untouched, which is
   what POSIX requires of a failed exec. Only after the load succeeds is
   anything destroyed.
3. **Quiesce the siblings.** `address_space_refs` reaching 1 is the proof that
   no other CPU can touch the space, because a thread only decrements it in
   `Thread::free`, after it has stopped running.
4. **Detach the old space and attach the new one in a single step, then tear
   the old one down.** `context_switch_to` reloads CR3 from `user.cr3` on every
   switch, so freeing a page table that is still published there hands a
   preemption a dangling CR3. Detaching first also means the blocking part of
   teardown (a `MAP_SHARED` writeback reaches the disk) happens with nothing
   half-swapped.

Three things underneath had to change, and are worth knowing independently:

- **`Thread::new_user` is split.** `load_process_image` builds an address space
  and an image; `new_user` attaches a new thread to it; `execve` attaches an
  existing process to it. The loader/process seam used to be welded shut.
- **`Thread::free`'s teardown is now `release_mappings`**, over a detached
  `MemoryManager` and `VmaSet`, and its descriptor shutdown is
  `pipe::close_descriptor`, shared with exec closing its close-on-exec fds.
- **A killed thread now dies at the syscall boundary.** `killed` was previously
  read on exactly one path — a PTY slave read — so a thread doing anything else
  ignored it. Kill was, in effect, "kill a shell foreground job blocked on
  input". A thread that makes no syscalls at all is caught by the timer tick
  instead (see below); one spinning inside the kernel is still nobody's to kill,
  which is why exec bounds its wait and refuses rather than assuming.

**Two traps if you touch this:**

- **Sibling threads are not keyed by `UserThread`.** `sys_clone` gives each
  thread its *own* `Arc<RwLock<UserThread>>` sharing the inner Arcs, so
  `Arc::ptr_eq` on the `UserThread` matches nothing. Address-space identity is
  the `address_space_refs` Arc. Keying on the wrong one made the quiesce find
  no siblings and time out, and `exectest`'s multithreaded case is what caught
  it.
- **`exectest`'s wake cases are the load-bearing ones.** Cases 1-3 pass with a
  broken quiesce; only case 4 exercises it. Reverting the cloexec close alone
  fails case 2, which was checked.

---

## There is an init process now

`bin/edos-init` is the only thing the kernel starts. It supervises `edos-wm`,
`edos-taskbar` and `edos-terminal` with a thread each — spawn, `waitpid`,
restart with backoff, give up after five rapid failures — so which programs
make up a session is userspace policy rather than something compiled into
`main.rs`.

Two consequences worth knowing:

- **A binary that fails to load no longer panics the kernel.** `boot_load_thread`
  used to `unwrap_or_else(|e| panic!(...))`, so a broken `/bin` took the machine
  down. It logs and leaves the kernel up; if *init itself* will not load, that is
  logged loudly and the serial console still works.
- **Killing the window manager is survivable.** `kill <wm pid>` and init restarts
  it; the desktop stays usable and input keeps routing, because windows live in
  the kernel registry and the new WM adopts them. This was the outcome I was
  least sure of and it works — verified twice, with a shell command typed into a
  pre-existing terminal window afterwards.

### Parentage, and the exit-status leak

Threads now carry the id of whoever created them, and so do exit statuses.
Before this, every exit inserted a status into `EXITED_THREADS` and only
`waitpid` removed one, so any process nobody waited on leaked a record forever.
When a creator dies, its children's statuses are dropped (nothing can name them
any more) and its surviving children are handed to init. `/proc/processes` has a
PPID column and prints the pending-status count, which stays at 2 across dozens
of spawns.

**The trap, and it cost a debug cycle:** this bookkeeping must not run on the
exit path. A registry walk plus two `Vec` allocations there hung the scheduler
suite at 48/49 — the exit path can run with interrupts disabled, which is
exactly what `reaper_enqueue`'s "must not allocate" comment warns about. It runs
in the reaper now, and `record_thread_exit` takes the parent from the dying
thread the caller already holds, so it neither allocates nor takes a lock. If
you add anything to thread exit, assume no allocation and no locks until proven
otherwise, and run `make test` — the failure was a timeout, not a panic.

---

## TCP works now, and the bug was in the waitqueue

`WaitQueue::wait_until_timeout` slept once and returned on any wake, without
re-checking the predicate or the deadline. Since a wake token left by an earlier
wait aborts the next sleep, `sys_connect` — which waits for ARP and then for
Established, back to back — had its second wait return in microseconds, decided
it had timed out, removed the connection and returned ECONNREFUSED. The SYN-ACK
landed 0.2 ms later, matched nothing, and got an RST. **No TCP connection had
ever been established in this kernel.**

`sys_read`'s socket paths had the same bug at the call site: one `wait_until`,
then treat an empty buffer as EOF, so every read returned 0 bytes.

Both fixed; `doc/bugs/2026-08-08-tcp-connect-rsts-its-own-synack.md` has the
detail. Verified with `tcptest` against a host HTTP server: a 270367-byte
response arrives intact, which finally exercises the RFC 6298 retransmit work.
`ping` also stopped losing its first packet to the same spurious ARP timeout.

**Two things to carry forward:**

- **Do not make the untimed arm of `wait_internal` loop.** It looks like the
  obvious symmetry and it stalls the boot: a caller whose predicate only becomes
  true through work that same thread has yet to do never returns. Two of three
  services failed to start. This has now looked correct twice.
- **When a container appears to lose an entry, instrument every mutation before
  theorising about the memory model.** The first investigation produced a
  genuinely alarming table — same address, coherent neighbouring atomic,
  `len=1` in one thread and `len=0` in another — and every observation in it was
  accurate. What was missing was a trace on connect's own `remove`. A reader
  that disagrees with a writer is far more likely to be a third writer you have
  not looked at.

---

## Fixed: two mappings shared a page, and one zeroed the other

Any `Vec` grown past 64 KiB came back full of zeros with its length intact.
It surfaced as a networking bug — `wget` saved 0 bytes of a 300 KB file —
and was not one: `read_to_end` collected all 300204 bytes correctly, and the
search for the `\r\n\r\n` terminator then found nothing, because the buffer
had been zeroed underneath it.

`VmaSet::reserve` searched for a gap of `length` rounded up to a page and
then recorded the VMA with the raw `length`. `first_fit` starts its next
search at a VMA's `end`, so a mapping that ended mid-page put the next one
*inside that same page*, and either could then destroy the other: a
zero-fill fault installs a fresh frame, and `munmap` of one unmaps a page
the other still uses.

The allocator hits it on the first chunk that is not exactly `CHUNK_SIZE`.
Growing to 128 KiB maps a ~131136-byte chunk starting 65600 bytes into the
previous chunk's last page; the copy lands there, the old chunk is freed,
and `release_chunk` unmaps the shared page along with it.

This was newly *reachable*, not newly written: the old bump allocator
returned page-aligned addresses by construction, and first fit made the
cursor follow VMA ends instead. `reserve` and `first_fit` work in whole
pages now, `sys_mmap`/`sys_munmap` reject an unaligned address rather than
rounding one silently, and `vectest` grows a `Vec` to 2 MiB verifying every
byte after each step. Full writeup in
[`bugs/2026-08-08-mappings-sharing-a-page.md`](bugs/2026-08-08-mappings-sharing-a-page.md).

**Two things worth carrying forward.** A correct length says nothing about
correct contents: every layer here reported the right byte count. And when a
buffer is zero *from offset 0*, suspect its backing pages rather than its
writer — a writer that skipped work leaves a hole, an unmapped-and-refaulted
page leaves a zeroed prefix.

## Fixed: the cwd mutex was taken with interrupts disabled

`info.lock().cwd.lock()` reads as two locks taken in sequence and is not:
the `UserThreadInfo` `IrqSpinlock` guard is a temporary that lives to the end
of the statement, so the cwd `BlockingMutex` was acquired with interrupts
off. Eighteen call sites did this. It panicked the kernel during boot, and
the CPU that died then stopped answering TLB shootdown IPIs, so a second CPU
panicked behind it with "never acknowledged a flush".

The same shape as the `sys_read` fd-table bug earlier in the session, and the
same lesson: *the assert fires only on contention, so a rarely-contended
wrong lock looks fine for months*. `current_cwd` / `set_current_cwd` clone
the `Arc` out of the guard first, and every call site goes through them.

## std::net is implemented, and that is where sockets belong now

`http` and `wget` were ported onto `edos_lib` first, which worked but was a
workaround: `std::net::TcpStream` returning "unsupported" was the actual
defect. Every wrapper std needed already existed in `edos_rt`, and every
syscall behind them already existed in the kernel (socket, bind, connect,
listen, accept, sendto, recvfrom, shutdown, get/setsockopt, getpeername,
getsockname) — nothing was wired to std, so the target fell through
`cfg_select!` in `sys/net/connection/mod.rs` to the unsupported stubs.

`library/std/src/sys/net/connection/edos.rs` in the fork implements
`TcpStream`, `TcpListener`, `UdpSocket` and `lookup_host`. Options the
kernel really has are real (timeouts, linger, nodelay, ttl, `SO_ERROR`);
the rest report unsupported rather than lying, and IPv6 is rejected rather
than truncated. `http`, `wget` and `dns` are plain std programs again, and
`edos_lib::http` is gone.

Verified: a 300000-byte file over `std::net` hashes identically to the
host's copy, and `http edgl.dev` fetches a real page off the internet by
name.

**The toolchain loop has a trap.** Bumping the `edos_rt` pin and running
`./x install` rebuilt nothing — bootstrap did not notice the lockfile
change, reported success in 24 seconds, and userspace kept linking the old
std. `touch library/std/src/lib.rs` forces it. A build that finishes far
too quickly after a dependency bump has not done what you asked.

## Resolution, and the query that still fails

DNS lives in `edos_rt::net::lookup_a` now, behind `ToSocketAddrs`. The
parser it replaced existed in two copies and desynchronised on a name that
ends in a compression pointer after its labels (RFC 1035 4.1.4), reading
the pointer's first byte as a length; that is why `dns edgl.dev` failed
while `example.com` worked. It also reports *why* a lookup failed instead
of answering every failure with "no A record", and the kernel now keeps the
resolver address DHCP offered (`SYS_GETDNS`) rather than parsing it into a
field nothing read.

**The first DNS query after boot used to get no reply, and the cause was
not the ARP drop it looked like.** `sys_recvfrom` did a single
`wait_until` and returned zero bytes if the queue was still empty. That
call returns on *any* wake, so a token left by an earlier wait aborted the
park and the receive reported an empty datagram immediately. The `sendto`
that triggers ARP is indeed dropped, but a correct receive would simply
have waited for the retry's answer.

It also explains why the resolver's retry did not rescue it: every attempt
returned just as fast, so the third read the *second* attempt's reply and
rejected it on the transaction id — which is why the error was "malformed"
rather than "no A record", and why chasing the parser was a dead end.

This is the contract the TCP read path was fixed for earlier in the
session; `recvfrom` and `accept` were the two places that kept the old
shape. Both loop on the real condition now, and `recvfrom` honours
`SO_RCVTIMEO`, which `setsockopt` had been storing with nothing reading it.
Verified on four cold boots. `programs/dnsprobe` dumps a raw response if
this area needs poking again.

## Checking a downloaded file from inside the guest

**Watch out.** memfs reads
past EOF and returns zeros to the end of the last page, so `sha256sum` of a
file on `/tmp` hashes the padding too and never matches the host, while
`stat` and `cat` both look right. The same file on `/var` hashes correctly.
Recorded in `todo.txt`; verify downloads on EFS until it is fixed.

## Fixed: a port restart stranded the op it meant to fail

The AHCI watchdog entry in `ideas.txt` proposed gating `enter_ncq_mode` on
`AhciPort.restarting`. That gate is not the fix. It keeps *new* submitters
out of a port being reset, and the op that strands is already past it.

`fail_all_ncq_slots` skips a slot whose `issued` is still false, on the
grounds that the submitter's own post-issue path will notice the generation
change. But `reset_generation` was bumped at the *end* of `restart_port`,
after that pass. A submitter that stored `issued` between the pass and the
bump, and sampled `SACT` before the reset cleared it, saw an unchanged
generation and its bit still set — so it returned and waited for a
completion nobody would deliver. A watchdog sweep found it up to 30s later.

The generation is published in `begin_restart`, before the fail-all pass,
and the submitter re-reads it after storing `issued`. The orderings are
complementary: either the submitter observes the bump and completes its own
slot, or its store precedes the pass, which fails the op. The gate went in
too, as a throughput measure.

**How it was validated, which matters more than the patch.** A real NCQ
command against a qcow2 backing file completes in well under a millisecond,
so no sane watchdog timeout is ever reached and the race never occurs
naturally — a 30 ms timeout produced zero firings under load.
`ahci_ncq_timeout_ms=0` on the kernel command line instead makes a sweep
treat *every* in-flight op as hung, so restarts land inside submits at
whatever rate I/O is running. `/proc/ahci_stats` gained `stranded`, which
counts ops a sweep finds still pending from an earlier generation — the
bug's exact fingerprint, and zero by construction once the ordering holds.

Under forced restarts with mixed read/write load: **1 stranded in 33
restarts before the fix, 0 in 106 after**. The pre-fix rate would have
predicted about three. Keep the injection in mind for any future work on
this path; the default timeout is untouched at 30s and the knob is inert
unless the command line sets it.

## A kill now reaches a thread that never enters the kernel

`killed` was observed at the syscall return boundary, which covers every real
program and misses the one case that mattered: a thread spinning in user code
makes no syscalls, so nothing ever asked it to die. `execve` had to bound its
sibling quiesce and refuse with EAGAIN for exactly that reason.

The timer tick checks the same flag now, and the condition that makes it safe is
**ring 3 in the interrupted frame**. There is no unwinding here, so a thread that
dies holding a lock guard leaks it permanently — that is the reader leak in
`bugs/2026-08-08-window-registry-stuck-reader.md`. A frame from ring 3 proves the
thread held nothing; a tick that caught it inside the kernel is left to the
syscall boundary, where the same `exit_if_killed` runs. Both callers share that
one function, so there is no second copy of the rule to keep in step.

Placement is in `tick_prepare`, before the tick touches the runqueue: the thread
is still Running, nothing has been published, and `thread_exit` pivots off its
kernel stack the way it does from a syscall. EOI has already been sent by then,
which matters — checking earlier would leave the ISR bit set on a CPU that is
about to run somebody else.

Two tests, and both were checked against the previous kernel:

- **`programs/killtest`** signals a child in each mode. Test 1 spins in user
  code, test 2 blocks in a syscall. It hands off through a pipe rather than a
  sleep, because killing a child still inside its runtime's startup would
  exercise the syscall boundary whichever mode was asked for, and it polls
  `waitpid_nonblocking` with a bound, because the failure is a process that never
  dies and a blocking `waitpid` would report that as this program hanging.
- **`exectest` test 5** execs from a process whose four siblings spin without
  syscalls.

Without the check, killtest test 1 reports the child alive 1000 ms after the
signal and exectest test 5 exits `EXEC_RETURNED`; exectest 1-4 still pass, so
test 5 is the only one that depends on it. With it: killtest 2/2, exectest 5/5,
threadtest + hammer + forktest clean, mmaptest 11/11 on both `/var` and `/tmp`,
49/49 in-kernel.

Still not covered, deliberately: a thread spinning **inside** the kernel. Nothing
can kill that safely, so `execve` keeps its bounded wait and its EAGAIN.

## Fixed: five more guards live across a user copy

`sys_window_list` was one instance of a class, and the sweep for the rest of it
found five more. The rule the class breaks: **a lock guard must not be live
across a user copy.**

Why the copy is a park point, which is the part that is not obvious: in the
ring-0 branch of `page_fault_handler`, `handle_demand_fault` runs *before* the
uaccess fixup, deliberately, so that a `try_copy_*` touching a lazily-mapped
page gets it mapped instead of failing. That handler blocks — NCQ I/O,
block-page-cache shard contention, vma waitqueues — with interrupts re-enabled.
EDOS has no unwinding, so a thread killed while parked there never runs the
guard's `Drop` and the lock is held for the life of the machine.

| Site | Guard | Consequence of a kill there |
|---|---|---|
| `Pipe::{write_from_user,read_to_user}` | `BlockingMutex<Pipe>` | that pipe wedges; every reader and writer parks forever |
| `Pty::{master,slave}_{write_from_user,read_to_user}` | `BlockingMutex<Pty>` | the terminal wedges |
| `tty::write_from_user` | `TTY_BUFFER` | stdout dies for every process |
| `vfs::read_to_user` (non-page-cache path) | `inode.lock` read | that procfs/devfs inode is unreadable |
| `vfs::write_from_user` (non-page-cache path) | `inode.lock` write | that inode is unreadable *and* unwritable |

The fix is one shape everywhere: **buffer first, lock second.** Writes copy out
of user space before taking the lock; reads drain into an owned `Vec` under the
lock and copy out after dropping it. `copy_in`/`copy_out` in `syscalls/io.rs`
are the helpers. The pipe and pty types lost their `*_from_user` / `*_to_user`
methods entirely, which is what stops the pattern coming back: the types no
longer know what a user pointer is.

Two deliberate trade-offs, both narrower than the leak they replace:

- **A read that faults on the copy loses the drained bytes.** It used to copy
  first and drain only on success. A fault here means the caller passed a bad
  buffer, and the alternative (peek, copy, then drain under a second
  acquisition) lets two concurrent readers see the same bytes.
- **TTY writes longer than 256 bytes may interleave with another writer's**,
  since the buffer lock is now taken per chunk instead of for the whole write.
  A TTY makes no atomicity guarantee above that.

Checked and already correct, because they snapshot into owned memory first:
`sys_ioctl`, `sys_window_poll`, `sys_list_mounts`.

Verified on a headless boot: `echo hello | wc -c` → 6, `ls /bin | wc -l` → 70,
`cat /proc/meminfo | head -3` (the exact vfs fallback path that was fixed),
`dmesg | wc -c` → 7328 bytes through the rewritten pipe path, killtest 2/2,
exectest 5/5, mmaptest 11/11 on `/var`, 49/49 in-kernel, no panic and no
shootdown timeout in the log.

### The regression guard, and exactly what it covers

`lock_order::assert_no_guards_held` is called at the top of `thread_exit`.
Every path that ends a thread funnels through there, so it is the one place the
rule can be checked, and it costs an `is_empty()` on a debug build.

**It covers ranked locks only**, because that is what the per-thread stack
records. The three locks the sweep touched that were unranked are now ranked, so
all six sites are covered: `TTY_BUFFER` 210, `Pipe` 220, `Pty` 230.

Those ranks are pinned by two constraints, and the reasoning is in
`invariants/lock-order.md`. Above 30, because `/dev/tty0` is a devfs device and
devfs has no `PageCacheOps`, so writing to it runs `TtyDevice::write` under
`inode.lock`. Below 900, because appending to any of these buffers allocates and
a heap expansion reaches the frame allocator. Nothing ranked is acquired while
one of them is held, which is the property to re-check before adding anything to
those critical sections.

Proven in both directions, since an assert never seen to fire is decoration:

- **Negative:** 49/49 in-kernel, then killtest, exectest, threadtest, forktest,
  iotest, mmaptest 11/11 and `lockordertest: PASS (inversions=0, max_depth=4)`
  on a booted desktop, plus `echo x > /dev/tty0` and `cat /dev/tty0` to force
  the `inode.lock` → `TTY_BUFFER` ordering. `/proc/lock_order_stats` reported
  `inversions: 0` throughout.
- **Positive, twice.** Pushing a fake rank in the `SYS_EXIT` arm panics on the
  first program exit: `thread 27 died at thread_exit holding 1 ranked guard(s),
  innermost 'positive-control' (rank 10)`. Then, after ranking, holding a real
  `TTY_BUFFER` guard across the exit panics with `innermost
  'tty::positive-control' (rank 210)` — which is the proof the widened coverage
  is real and not just a bigger table. Both reverted.

Worth knowing when reading this class: **the hang that opened the entry in
`ideas.txt` was re-diagnosed as starvation**, not a leaked guard, by
`bugs/2026-08-08-window-registry-stuck-reader.md`. The class has never been
caught in the act. It was swept because the mechanism is provable by
inspection, not because that deadlock was an instance of it.

Where the rule can be broken at all is narrower than "anywhere a thread dies".
Every ring-3 kill point (GPF, invalid opcode, alignment check, page fault, the
timer tick) interrupts user code, where the thread provably holds nothing;
`exit_if_killed` runs after the syscall body returned and dropped its guards; a
ring-0 uaccess fault takes the fixup and returns EFAULT rather than killing.
That leaves explicit `thread_exit()` inside a syscall body, of which there are
two, both currently safe.

## Lock-order ranks now cover IPC, networking and the window system

Three subsystems ranked on top of the FS/MM ladder that Foundation #4 shipped:
TTY/pipe/pty 210-230, networking 240-270, window system 280-300. The rank table
in [`invariants/lock-order.md`](invariants/lock-order.md) is authoritative and
has the per-lock reasoning; this is the summary of what it bought.

**Networking was the payoff: two pre-existing AB/BA inversions**, both shaped as
"take the port table while holding something that belongs inside it".

- `tcp_retransmit_main`'s cleanup freed the ephemeral port inside the `retain`
  closure with the connection guard live, closing the cycle
  `PORT_TABLE -> SOCKET -> TCP_CONN -> PORT_TABLE`. It never deadlocked only
  because the socket held under the port table in `handle_tcp` is always a
  *listening* one, whose `poll_state` reads the accept queue instead of locking a
  connection. Nothing enforced that.
- `close_descriptor`'s socket arm took the port table under the socket guard,
  against the receive path's opposite order. This one needs no invariant to
  break: closing a listening socket while a segment arrives for it wedges two
  CPUs on preempt spinlocks — a syscall against the e1000e rx kthread.

Both now collect what they need under the guard and release it after. **Neither
is visible by reading either function alone**; the rank system found them
because no total order existed over the observed nestings. That is the argument
for doing this to a subsystem at all.

**The window system was already consistent** — no inversions. Worth recording so
nobody re-derives it: `handle_mouse_event` already drops its read guard before
upgrading to a write lock, `cleanup_process_windows` already scopes its guard,
and the event-queue side never reaches back into the registry.

**Ranking is also what makes a lock visible to `assert_no_guards_held`.** That
assert only sees ranked locks, so the pipe/pty/TTY ranks exist as much for the
dying-thread check as for ordering. If you add a lock that a syscall can hold
across a park, rank it even if it is a leaf.

Validation was the tracker itself, which panics on a wrong rank rather than
passing quietly: 49/49 in-kernel, a booted desktop with DHCP/ARP/ping/DNS and
repeated `http` fetches over the real internet, a synthetic click-and-type soak,
and `lockordertest: PASS`. `/proc/lock_order_stats` read `inversions: 0`
throughout. Note what that does *not* show: it proves the new order is
self-consistent under load, not that the old code would have deadlocked. The
case for both bugs is structural, from the code, not from a reproduction.

## USB, shared memory and the input path are ranked too

The follow-up sweep (2026-08-10) added ranks 204/206 (`Mailbox.queue`,
`ResponseInner.value`), 90 (`SHARED_MEMORY_REGISTRY`) and 310/320
(`Broadcaster.subs`, the `/dev/kbd` + `/dev/mouse` poller lists). No inversions
appeared; `/proc/lock_order_stats` read `inversions: 0, max_depth: 3` on a
booted desktop after mmaptest 11/11, forktest, lockordertest, and a window
opened and closed to drive the shm teardown path.

**USB has no locks of its own, and that is the result rather than a gap.**
`XhciController` is only ever `&mut self` inside its driver thread; the MSI-X
handler just wakes that thread. Every other thread reaches it through a channel,
so what the sweep actually ranked was the channels — a mailbox shared with the
FS mount path, and broadcasters shared with PS/2 input.

**The shm registry's old rationale was wrong, and that is why it stayed
unranked.** `invariants/lock-order.md` said it was never co-held with vmas (70)
or mm (80). That was an audit of `syscalls/shm.rs` alone: `sys_fork`'s deep copy
resolves each SHM VMA's region under the vmas guard, and `release_mappings` does
the same under the page-table guard, which `Thread::free` holds across the whole
call. Rank 90 sits inside both. The first attempt at 75 (inside vmas, outside
mm) is wrong for exactly that second reason.

**Two real defects came out of the USB half**, neither of them an ordering bug:

- `Broadcaster.subs` was a bare `spin::RwLock` shared between driver kthreads,
  the window input thread and syscall context — a descheduled holder stalls
  every other CPU, the shape of the window-registry hang. Now `PreemptRwLock`,
  and `subscribe` builds its 256-slot `ArrayQueue` before taking the guard
  instead of under it.
- The USB HID paths broadcast to subscribers but never notified pollers, while
  the PS/2 paths did. Since `USB_*_ACTIVE` suppresses the PS/2 producer, `poll()`
  on `/dev/kbd` or `/dev/mouse` never reported readable with a USB device
  attached — which is the default machine. Both halves now sit behind
  `dispatch_key_events` / `dispatch_mouse_event`.

**Audio and devfs finished the list (same day).** `HdaPlaybackState` is rank
330 and devfs's `DevFs.shared` is 340. Both were bare spin locks over
thread-shared state, the same primitive error as `Broadcaster.subs`: HDA's was
held across a memcpy loop into the DMA ring, between `/dev/dsp` writers and the
audio kthread. `TTY_POLLERS` also joined the device-poller class at 320.

**Ranking devfs paid for itself on the first `ls /dev`:**

```
lock order violation: tried to acquire 'tty::device_size' (rank 210)
while holding 'devfs::list_files' (rank 340);
full stack: [inode.lock(30), devfs::list_files(340)]
```

`read_bytes`, `write_bytes`, `ioctl`, `poll` and `mmap` all release the registry
guard before calling into a device. `list_files` and `file_info` did not,
because their call into the driver does not *look* like a dispatch:
`DeviceNode::file_entry` reads `DevFsDevice::size`, which for `/dev/tty0` takes
the rank-210 `BlockingMutex`. That is a spin lock held across a lock that can
park. Both snapshot the nodes under the guard and build their `File` entries
after it now. Ranking the registry *above* the device locks is what makes the
mistake loud; ranking it below would have been legal and silent.

**`scripts/edos-vm` had no audio device at all**, so `hda: no device found` and
the driver never initialized — the primary way this OS gets exercised could not
test audio. It now passes `-audiodev none,id=snd0 -device intel-hda -device
hda-output,audiodev=snd0`. `none` rather than `pipewire`: the guest DMA engine
and interrupts run either way, and pipewire refuses to start without a session
bus, which is the exact case that script exists for.

Still bare `spin::Mutex`/`RwLock` over thread-shared state, worth the same
treatment and not yet audited: the `log` ring buffer in `logs.rs` (careful, it
must stay reachable from paths that cannot take locks), `random.rs`'s RNG state,
`PCI_MANAGER`, and `ALLOWED_PHYS_RANGES`. The scheduler's own locks,
`PCI_CONFIG_LOCK` and the AHCI slot/mmio locks stay bare on purpose.

**A trap worth naming: rewriting lock calls mechanically can drop a `!`.**
Wrapping `wait_until(|| !self.queue.lock().is_empty())` in `ranked_lock!` lost
the negation, and the kernel hung at boot right after the root mount with the
serial log simply stopping — the FS mailbox thread waiting on an inverted
predicate. The symptom looks like a deadlock in whatever ran last, not like a
typo. Re-read predicates after a macro rewrite.

## Ctrl+C kills the foreground job, and always did

Verified in the VM on 2026-08-10: `sleep 30` and a stdin-blocked `cat` both die
on Ctrl+C with the prompt returning, and Ctrl+C at an idle prompt leaves the
shell alive. `ideas.txt` claimed the kill delivery behind `LineAction::Interrupt`
was the one missing piece; it was already there (`PtyNotifications::kill_pid` ->
`flush()` -> `kill_process`) and the entry had gone stale.

**What keeps the shell alive is not the foreground bookkeeping.** `sys_spawn`
registers any child whose fd 0 is a PTY slave as `foreground_pid`, including the
session shell the terminal spawns, so at an idle prompt Ctrl+C really is
delivered to the shell. `edos-sh` sets SIGINT to SIG_IGN at startup and
`kill_process_with_signal` returns early on SIG_IGN. A negative-control kernel
that registers the shell unconditionally still leaves it alive, which is how
that was established — a plausible-looking "the shell would be killed" fix was
built, refuted by the control, and reverted.

## Ctrl-D ends a stdin read now

`Pty::slave_read` returned an empty `Vec` for two different things — Ctrl-D
(`eof_pending` consumed) and "no data yet" — so the caller could not tell them
apart and parked in both cases. A program reading stdin (`wc`, `sort`, `cat`
with no args) therefore hung with no way out unless the master closed. It
returns `PtySlaveRead::{Data,Eof,WouldBlock}` now, and `sys_read` breaks with 0
on `Eof`, which is how POSIX spells EOF.

Verified against a negative control, because the first end-to-end test was wrong
in a way worth recording: with `Eof` folded back into no-data, `wc -l` hangs and
the next command is swallowed as stdin; with the fix it prints the count and
returns to the prompt.

**The userspace chain was never broken, and a too-narrow grep said otherwise.**
`grep ctrl programs/edos-terminal/src/` finds nothing, which looks like "the
terminal has no ctrl handling". The handling is one layer down:
`edos_lib::keymap::map_keycode` maps ctrl+a..z to 0x01..0x1a and the terminal
*widget* in `edos_render` tracks the modifier. Grep the widget and the keymap,
not just the program.

**And `scripts/edos-vm key` splits combos on `+`, not `-`.** `key ctrl-d` sends
one bogus qcode and silently does nothing, which reads exactly like a missing
feature. `key ctrl+d` is correct, as the script's own help says.

## The syscall table is closed, and closing it found five data-loss bugs

`doc/AUDIT.md` §3 listed eight missing interfaces; all eight now exist and the
table is down to `setuid`, which is rejected there. 101 syscalls, each with an
`edos_lib` wrapper and a case in `programs/iotest` — **`iotest /var` is the
regression suite for the whole set, and it runs 18/18.**

The syscalls are not the interesting part. Writing them found five bugs that
predate them, every one a silent data corruption:

- **`VfsInode` identity was keyed by the dentry cache**, so any invalidation
  (truncate, rename, create, or the LRU at 256 entries) forked one file into two
  inodes with independent page caches. A dirty page stayed on the first and read
  back as zeros through the second, then landed on disk over newer data. Inodes
  are keyed `(mount_id, ino)` through `fs/icache.rs` now.
- **Every EFS timestamp was 93 days late** — the shared days-from-civil helper
  used `(153*m+8)/5` instead of `(153*(m-3)+2)/5`.
- **memfs kept two sizes**, so every short `/tmp` file reported and read back
  padded to its last 4 KiB page.
- **An EFS hole read as `Corrupted`** rather than zeros, and growing an inline
  inode past the 176-byte inline area panicked the kernel.
- **No filesystem checked whether a name was free**, so `mkdir`/`create`/
  `symlink` over an existing entry added a *second* directory entry with the
  same name.

The lesson worth carrying: each was found by writing the syscall that exercised
the layer, not by reading the layer.

## One defect class, found in three drivers

**A pooled DMA buffer is not zeroed on reuse, and every parser read a fixed size
without asking how many bytes arrived.** A short transfer therefore returned the
previous owner's bytes as device identity or as sector data. Fixed in xHCI
descriptors (`7591982`) and USB mass storage (`41e2c41`, where `block_size == 0`
also faulted the CPU and an oversized one made `read_sectors` loop forever).

**AHCI ATAPI has the same defect and is still open.** `execute_atapi_command`
drops the count the command header's `prdbc` already carries. It is verifiable
today with no new QEMU option: `-cdrom` on q35 lands on the ICH9 AHCI
controller, so the guest logs `Found ATAPI device on port 2` /
`Model: QEMU QEMU DVD-ROM` on every boot. `todo.txt` has the fix and the recipe.

If you add a driver that reads out of `DmaPool`, this is the first thing to
check. `allocate_sized` does not zero, and documents why: it serves AHCI
per-command buffers up to 2 MiB, so a memset per pop is a storage regression.

## The shell was rebuilt, and the kernel gave two things back to userspace

The GUI now has proportional type (Lato for chrome, JetBrains Mono for the
grid, from `/share/fonts` via `fontdue`), a panel with launcher/tasks/status
regions and icons, an applications menu with working power controls, minimize
and maximize, and a desktop right-click menu. `programs/wintest` is the
reference for the widget toolkit and now models a disabled state and aligned
columns.

Two moves that matter beyond the pixels:

- **The kernel no longer knows what a title bar is.** It routes pointer events
  into client space, so it needs the offset -- but each window now carries the
  frame *its manager gave it*, through `property::FRAME`. There is no global
  decoration constant in the kernel, and different windows can be framed
  differently, which is what a menu needs.
- **`FLAG_DOCK` split into `FLAG_UNDECORATED` and `FLAG_NO_FOCUS`.** They were
  one flag, and a menu needs the first without the second: it has no title bar,
  and it must take focus because losing focus is how it closes.

- **Managing another process's window needs a privilege now.** It was ungated:
  any process could move, resize, minimize or post a close event to any window.
  Init holds the privilege by being the process the kernel starts, and grants it
  per spawn to the compositor and the panel (`kernel/src/window/shell.rs`,
  `SYS_WINDOW_GRANT_SHELL` 234). Two things fell out of writing it: the
  privilege has to follow a process's *threads*, because `pid` here is a
  thread's own id and there is no thread-group id, so a grant is propagated at
  `sys_clone`; and the shell table must be ranked *outside* the window registry
  and settled before it is taken, which the lock-order tracker caught on the
  first boot.

Traps this round produced, both of which cost a build cycle:

- **`WidgetContainer` wraps every widget to assign it an id and forwards each
  trait method by hand.** A method added to `Widget` with a default body is
  inherited by the wrapper and never reaches the real widget. It compiles, it
  looks right, and it silently does nothing.
- **A window created this frame is not in the window list the caller already
  fetched.** The panel's menu closed itself instantly because its absence from
  a stale list read as "destroyed".

## The shell's loose ends, closed

Four things the rebuild left open, and what each turned out to need.

**Windows are addressable by name.** `/proc/windows` publishes the kernel
registry, and the compositor copies that file into the kernel log on
`Ctrl+Alt+W`; the serial console is the only channel out of a headless guest, so
that keystroke is how the geometry reaches the host. `scripts/edos-vm windows`
and `focus <title>` are the host side.

Two details that are the whole difference between this working and looking like
it works:

- **The reported origin is the *outer* one and the reported size is the
  *client* one**, with the frame as a separate column, because that is what the
  kernel routes pointer events by. Clicking `x + w/2, y + h/2` lands in the
  title bar of a tall window and on the desktop below a short one.
- **Clicking a window's centre focuses whatever is on top of it.** `focus`
  subtracts every higher-z window's rect from the target's client area and
  clicks a point that survives, which is what raises a partly covered window;
  the first version clicked the centre and confidently focused the wrong window
  while reporting the right name. A fully covered window is reported, not
  guessed at.

**Wallpapers.** `edos_render::image` decodes 24- and 32-bit uncompressed BMP and
scales to cover; the compositor cycles the three generated lit grounds and every
readable `.bmp` in `/share/wallpapers` through the one desktop-menu entry. The
shipped image is generated by `scripts/mkwallpaper.py` at build time, since this
repo holds no binaries, and the make rule depends on the script so an unchanged
wallpaper keeps its timestamp — the disk-image manifest is timestamp-based, so
regenerating it every build would rebuild both images every build.

**The status area does something.** Volume drives the HDA output amps through
two new `/dev/dsp` ioctls; the gain scale comes from the codec's own Output
Amplifier Capabilities rather than a hardcoded `0x7F` (QEMU's reports 74 steps),
and zero mutes rather than attenuating to the quietest step. Network reports
link, address, gateway, resolver and MAC from a new `/proc/net`. That file
exists because `SYS_NETINFO` renders the same state *for a terminal*, ANSI
colour codes and all, and a UI parsing that would be reading a display format.

**`std` reaches the whole syscall table** (`edos_rt` 0.0.42, fork pin bumped):
symlinks, file times, `is_symlink`, vectored I/O, `nanosleep`, a `ReadDir` that
streams through `getdents` a chunk at a time instead of demanding a buffer for
the whole directory, `access` behind `try_exists`, and `openat` so an open no
longer allocates a `CString`.

One of the nineteen was not a wrapper. `File::set_times` needs to stamp a file
the caller holds *open*, and `SYS_UTIMENSAT` took a path; a `File` has only a
descriptor. The kernel grew the POSIX form — a null path means the file `dirfd`
names — which is `futimens`, covered by `iotest` test 9.

## Fixed: a new window was black until its client painted

Reported from a VNC session, and invisible to a screenshot taken a moment
later. A window was created **mapped** (`WindowInfo::new` set `visible: true`)
and `Window::new` immediately pointed the compositor at buffer 0 — a buffer
nobody had drawn into. Everything between `window_create` and the client's
first frame — allocating the second buffer, the title, the flags, the client's
own pre-render — was therefore composited as a black rectangle inside real
decorations.

Both halves had to go: a window is created unmapped and its client maps it with
`show()`, and no buffer is published until the first `swap_buffers`, which is
the only call that means "this is what I look like". A window with no buffer
composites as its own themed ground, so a client that maps before painting
costs a frame of empty window rather than a black hole.

`Window::resize` still publishes an unpainted buffer, deliberately: the old
pair is freed immediately after, so the alternative is leaving the compositor
holding a freed shm id.

## The USB HID driver reads report descriptors now

It bound a device on `bInterfaceClass == HID && bInterfaceProtocol == 1|2` and
then decoded one fixed layout, so it understood exactly two devices: a boot
keyboard and a boot mouse. Those protocol codes only mean anything on an
interface that declares the *boot* subclass, so `usb-tablet` — which declares
none — enumerated and was dropped, and the guest had no absolute pointer. Under
VNC that shows up as the host pointer drifting away from the guest cursor and
walking out of the window, which is a symptom two layers above the cause.

`drivers/usb/hid/report.rs` parses the item stream into a field map: bit
offset, width, signedness, usage, and whether the value is a position or a
displacement. That last flag is the whole difference between a mouse and a
tablet and it is stated by the Input item; nothing about a byte layout implies
it. A pointer is now bound because its descriptor says it has X and Y.

Things worth knowing if you touch it:

- **The boot decoder is still there, as the fallback** for a descriptor that
  will not parse. A device the driver used to handle must not be lost to a
  parser bug.
- **`SET_PROTOCOL` is only sent when the fixed layout is what will be decoded**,
  and only to an interface that declares the boot subclass. Asking a tablet for
  boot protocol stalls, and asking a mouse for it after reading its report
  descriptor would replace the layout that was just parsed.
- **The report length comes from the endpoint descriptor**, not from the four
  bytes the boot layout happens to use: a tablet reports six.
- `parse_pointer` only reads inside the collection that declares itself a
  pointer or a mouse. A keyboard descriptor can carry an X/Y pair in a vendor
  collection, and taking it would make the keyboard the pointer.
- The sched-test suite parses both descriptors QEMU emits and checks the
  decoded offsets, values, scaling and the absolute flag, plus that a keyboard
  and a truncated descriptor are both refused. 49 → 50 tests.

**A trap that cost a debug cycle, and it was in the host script.** QMP serves
one client at a time. `pointer_is_absolute()` opened its own connection while
the caller already held one, so it timed out, reported "not absolute", and the
script silently fell back to relative motion — which QEMU *does* apply to a
tablet, so the pointer still moved and only the clicks went missing. It takes
the caller's connection now.

## The cursor moved to its own plane

Reading report descriptors gave the guest an absolute pointer, which is what
the hardware cursor had been waiting for: `hw_cursor` in the window manager was
hard-coded `false` with a comment saying so. With it on, the compositor stops
painting the pointer into the framebuffer, so moving the mouse damages nothing
and costs one small message; a remote viewer is handed the image and draws it
at its own pointer speed. That is most of what "the mouse is not smooth" over
VNC was.

The cursor texture already had zero alpha where it is transparent, which is
what both the software blit and the cursor plane want, so there is one cursor
image rather than two. A shape change is an upload rather than a different
texture at composite time, and the flag falls back to the software cursor if
the display has no cursor plane to take it.

**`screendump` does not capture the cursor plane**, so screenshots no longer
contain a pointer. That is worth knowing before it is read as a pointer that
failed to move; it also means a screenshot is no longer a way to check where
the pointer is.

## What a frame costs, measured

Asked whether dragging a window was slow, and whether the hardware cursor was
covering for a slow compositor. It is not. `FrameStats` in the window manager
times the composite and the transfer and reports only when frames miss their
budget, so it is silent on a healthy machine.

Dragging a 640x480 window across a 1920x1080 screen for five seconds, KVM,
four cores:

| | |
|---|---|
| frames per second | 77, against a 74Hz target |
| composite | 1.56 ms average, 2.4-4.9 ms worst |
| transfer (`flip_rect`) | 0.4 ms average |
| frames over the 13 ms budget | **0** |

So the guest composites a drag in about 2 ms of a 13 ms budget and misses
nothing. What a VNC viewer shows is the remote-framebuffer limit: a moving
640x480 window damages its old and new rectangles, some 2.4 MB of pixels per
frame, and that has to be encoded and shipped. The cursor became smooth
because on its own plane it ships *no pixels at all*.

Then the same counter was asked what the *display* is being handed, because a
guest that hits its frame rate can still be producing more than a remote
viewer can carry. Dragging that window:

**~250 MB/s of raw pixels**, about 3 MB per frame at 77 frames a second.

That is the whole story of "dragging is not smooth over VNC/SPICE". A moving
window's old and new rectangles both change, so the damage is roughly the
window's area every frame, and a remote protocol has to compress and ship all
of it. Gigabit ethernet carries 125 MB/s. The viewer is oversubscribed two to
five times over, so it applies updates partially -- which is what reads as
tearing, and it shows up first on a title bar because that is the crispest
edge on screen. The guest is presenting whole frames: `transfer_and_flush`
polls both commands to completion before returning, so nothing is being drawn
into while the host reads it.

This is also what SPICE's `streaming-video=filter` is for: it re-encodes a
fast-changing rectangle as lossy video so it *fits*. Turning it off buys
sharpness and spends smoothness. There is no setting that buys both, and no
change inside the guest that makes a moving window stop being megabytes.

Two things the numbers say that are worth keeping:

- **The first frames are enormous** — one report at boot averages 240 ms with a
  2.16 s worst case, while fonts load and the shm buffers fault in. It is the
  slow first paint at boot, not a steady-state problem.
- **Cost scales with the screen, not with what changed.** The compositor
  rewrites all 1920x1080 pixels every frame and only limits the *transfer* to
  the dirty rectangle. 1.5 ms says that is affordable today; it is where to
  look first if it stops being.

## A filesystem cannot resolve a symbolic link, and now does not try

`iotest /tmp` stopped at test 10 with "read through link: entity not found"
while the identical `iotest /var` passed. The VFS hands each filesystem a
*mount-relative* path and each filesystem resolved link targets from its own
root, so a link at `/tmp/link` naming `/tmp/target` made memfs look for
`tmp/target` under the memfs root, which has no `tmp`. EFS only worked because
it is mounted at `/`, where mount-relative and absolute coincide.

Chasing the fix turned up two more of the same shape, which is why the answer
is broader than the symptom. A relative target can walk *out* of its mount
(`/tmp/l -> ../var/x`), and the filesystem clamps the `..` at its own root
instead. And a target that stays put can still cross into something mounted
*deeper*, which the filesystem also cannot see. There is no rule by which a
filesystem gets any of these right, because the mount table is not its to
read.

So a filesystem no longer resolves a link target at all. Its walk stops at the
first link it is asked to follow and reports `Error::LinkEscape`; the VFS asks
where the link pointed (`FileSystem::link_escape`, answering in the only terms
a filesystem has: an absolute target, or a relative one plus how many levels
above the mount point it started), turns that into an absolute path, and
restarts resolution from the VFS root. The hop cap lives in the VFS now, so it
counts hops across mounts rather than per filesystem.

Two consequences worth knowing:

- **Escalation is error-driven, not a pre-pass.** `fs::api::with_links` runs
  the operation and only redirects when it comes back `LinkEscape`, so a path
  with no symbolic links costs exactly one walk, as before. Probing each prefix
  with `read_link` would have been the obvious shape and is O(N) walks per
  lookup.
- **The follow/nofollow distinction had to move up.** It used to live inside
  each filesystem's walk. `LinkMode` now carries it from the API layer, because
  the redirect has to be computed the same way the operation walks: `unlink`,
  `readlink`, `symlink` and `rename` leave the final component alone, and
  everything else follows it. `rename` is the one operation holding two paths,
  so a retry could not say which side raised the error; it settles both with
  `resolve_links` before calling.

`open` caches the path on the descriptor, so it takes the resolved one:
`file_info_resolved` hands back the path it landed on, which differs from the
one asked for exactly when a link crossed a mount.

## The panel publishes where its own buttons are

`scripts/edos-vm launch` used to mirror `programs/edos-taskbar/src/{main,panel,
menu}.rs` by hand, because the panel's buttons are not windows and nothing in
`/proc/windows` accounts for them. Moving the layout silently misaimed every
scripted click: no compile error, no failing test.

The panel writes them out itself now, the same way the window manager copies
`/proc/windows` into the kernel log: `panel|` lines whenever the layout moves
(a window opening or closing, or the clock growing a digit), and `menu|` lines
as the applications menu opens. `klog_dump` in `edos_lib::io` is the shared
writer. `scripts/edos-vm` grows `panel` and `press <name>`, `launch` resolves
rows by label, and every layout constant is gone from the script.

The panel needs no request channel because it republishes on change, so the
last block in the log is current. The menu does: it exists only while open, so
`launch` notes where the log ends before clicking the launcher and only reads
what lands after that.

## What the symlink rework broke, and what that says about the test suite

A review of the finished diff found four regressions, all of the same shape and
none caught by `iotest` passing on both filesystems. Worth writing down, because
the shape is the lesson: **making a filesystem report an escape instead of
resolving it turns every caller that did not expect an error into a caller that
now fails.** The retry loop covers `fs::api`. Anything reaching the VFS by
another door does not.

- **Executing through a symbolic link stopped working.** `fs::api::resolve_inode`
  is how the ELF loader reaches a binary — `do_spawn`, `execve`, and the boot
  load of `bin/edos-init` — and it called `vfs::resolve` directly, outside the
  loop. `ln -s /bin/ls /bin/ll; ll` failed with ENOEXEC while `cat /bin/ll`
  worked, because the shebang probe goes through `read_bytes`, which retries.
  A wrong errno on a path that demonstrably exists is the tell.
- **`rename` and `rmdir` on EFS returned ELOOP and EIO.** Both resolved their
  target with the *follow* variant while `fs::api` asked for nofollow. Two
  pre-existing bugs fell out of fixing that: `mv link newname` used to make
  `newname` a second name for the link's *target*, and `rmdir symlink-to-dir`
  used to free the target directory. memfs had it right all along.
- **`open(O_CREAT)` through a symlinked directory left a permanently broken
  fd.** `create_file` retries and creates the file at the resolved path;
  `open` then cached the *unresolved* one, so every later read and write on
  that descriptor failed.

The general hazard the design carries: `link_escape` is asked with the *API's*
link mode, not the mode the filesystem operation actually walked with, and the
two agree only by convention. Every op-follows / api-nofollow pair produces
`Unsupported`, which surfaces as EIO. `rmdir` was the only live instance; a
filesystem operation added later that follows a final component the API says to
leave alone will do it again.

`iotest` now covers all four: exec through a link, create-write-read through a
linked directory, rename of a link, and `rmdir` refusing one. Plus a two-link
cycle, which is the case that proves the new loop terminates rather than hangs.

## procfs answers for per-process memory

Writing a graphical process viewer turned up the gap: nothing anywhere said how
much memory a process was using. The closest was the VMA *count*, which says
how many mappings exist and nothing about their size.

`/proc/processes` has an RSS column now and `/proc/<tid>/status` a `VM Size` and
a `Resident` line. Virtual size is the sum of the VMA lengths and is free.
Resident is counted from the page tables when read, and that is the decision
worth recording: a page enters a user address space from demand paging,
copy-on-write, `mmap`, shared memory and the loader, and leaves it from a dozen
`unmap` sites, so a counter maintained at each of them drifts the first time one
is missed — and a memory number that is quietly wrong is worse than no memory
number. The walk descends only into *present* entries, so the lazily faulted
mappings this kernel leans on cost one skipped entry rather than a probe per
page; probing each page of each VMA instead would have been O(virtual size),
which for a sparsely faulted mapping is most of the work for none of the answer.

The lock order is `vmas` (70) then `memory_manager` (80), in that order.

Holding the manager is not on its own enough to make the walk safe, which was
the other thing the review caught. The reaper calls `Thread::free` *before*
dropping the thread from the registry, and procfs snapshots the registry into
`Vec<Arc<Thread>>` first, so it can reach a `MemoryManager` whose PML4 frame is
already back in the allocator and possibly reused — and `mapper` is an
`OffsetPageTable<'static>` whose lifetime says nothing about that. Reading the
VMA count was safe because a Rust structure stays allocated; this is the first
reader that follows the raw frame pointer. `Thread::free` now calls
`release_page_tables()` under the mm lock before freeing the frame, and
`resident_bytes` returns 0 once that is set.

A first reading, `/bin/edos-wm`: 471 VMAs, 51100 KiB of address space, 42660 KiB
resident. `/bin/sh` 208 KiB and `/bin/ps` 60 KiB resident against ~300 KiB
binaries, which is demand paging visible in a number for the first time. Kernel
threads report `-` rather than a figure: they have no address space of their
own, and reporting the kernel's would be a lie that adds up.

## strace exists, and it is now the first thing to reach for

A program that failed silently used to leave nothing behind — this OS is driven
through screenshots and a serial log, so "it printed nothing" was the end of the
evidence. `strace` makes it the beginning. Full write-up in
[`strace.md`](strace.md); the parts worth knowing before reading code:

**It is not ptrace, and deliberately so.** `syscall_handler` is a single choke
point for the entire syscall surface, so tracing is an entry record before the
match, a return record after it, and a per-thread mark to decide whether to
write either. Nothing stops the target and nothing changes its scheduling.

**The mark is a generation, not a bool.** `Thread::traced` holds the trace
session it was marked under, and only counts while that equals the live
generation. Ending a session is therefore one increment rather than a walk of
the thread table, and a mark a dead tracer left behind cannot reactivate under
the next one. This matters more than it looks: a stale mark means a program
writing records into a ring nobody drains, forever.

**A tracer that dies releases the session**, because `thread_exit` calls into
the tracer for the `+++ exited +++` record anyway. Ctrl+C on `strace -p` leaves
nothing marked, which is verified behaviour and not an assumption.

**Records can be lost and the count is printed.** The target never blocks on the
tracer; a ring that fills drops and counts. A tool that silently omits calls is
worse than one that admits it.

**Three things the design bought that are worth keeping in mind:**

- `/proc/syscalls` publishes the kernel's own syscall table (number, name,
  argument kinds) and its errno names, so `strace` holds no duplicate that could
  drift the way `WindowListEntry` can. **Adding a syscall now means adding a row
  to `kernel/src/syscalls/table.rs`** or `strace` will print it as
  `syscall_NNN(0x…, 0x…)`.
- Buffer contents are captured on both sides: an input buffer on entry, an
  output buffer on return, sized by the return value. The output side finds its
  buffer through the arguments *copied at entry* and carried in a `TracedCall`,
  not through the registers as they stand on return — `sys_execve` rewrites the
  whole `SyscallContext`, so those can name a dead address space. An earlier
  draft relied on "the dispatcher only ever assigns to `ctx.rax`", which is
  false. That is what makes `write(1, "hi\n", 3)` and `read(3, "…", 4096) = 12`
  readable.
- A call still in flight prints `<unfinished ...>` and resumes later. `strace -T
  sleep 1` showing `<... nanosleep resumed> = 0 <1.000049>` is the answer to
  "the program is hung", not a guess about it.

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
- **`make all` does not rebuild `sata-disk.img`**, and every `run` target
  attaches it and prefers it over the live-root ramdisk. A rebuilt program is
  invisible to the guest until `make sata-disk.img`, so a screenshot looks
  exactly as if the change did nothing.
- **`make sata-disk.img` fails while a VM is running** — `qemu-img` reports
  "Failed to get write lock" — and the guest then boots the old binary. Stop the
  VM first.
- **`make test` leaves the sched-test ISO in place.** A later `edos-vm start`
  boots the test kernel rather than the desktop; re-run `make all` before manual
  guest checks.
- **`cargo check` from the repo root uses the wrong toolchain.** The root
  `rust-toolchain.toml` says plain `nightly`, `kernel/` pins
  `nightly-2026-03-06`, and the `x86_64` crate does not build on current
  nightly. Use `make -C kernel check`.
- **`efs-fsck` aborts before its dir-tree pass on a dirty journal**, so a "0
  findings" line from a power-cut image proves nothing. Type `shutdown` in the
  guest rather than `edos-vm stop`: it syncs every filesystem and the resulting
  image checks clean with no `--repair` replay.
- **Nothing on screen is addressed by pixel any more.** Windows go by title
  (`edos-vm windows`, `edos-vm focus <title>`) from `/proc/windows`; the panel's
  controls go by name (`edos-vm panel`, `edos-vm press <name>`, `edos-vm launch
  <row>`) from what the panel itself publishes. No layout constant is left in
  `scripts/edos-vm`, so moving the panel no longer silently misaims every
  scripted click. A minimized window still has no geometry to click: `press
  <title>` hits its task button, which restores it.
- **The sched-test suite has a known flake with two signatures**, both on a
  first run: `ping-pong count mismatch: 499 != 500`, and 48/49 TIMEOUT with
  ping-pong-pong never reporting. An immediate re-run passes. Recorded in
  `todo.txt`; it points at a lost or late wakeup, and it has never been chased.
