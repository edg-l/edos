# Scheduler roadmap

What to do next to the switch and wake paths, in priority order, with the
evidence for each. Measure with `programs/switchbench` and
`/proc/sched_prof`; `doc/WORKING-NOTES.md` holds the record of what has already
been done and what the numbers mean.

**Take every number on a single-CPU boot.** `switchbench` needs the two
threads it measures to be on one CPU — give the scheduler a second and it puts
them on both, where neither ever waits for the other and every handover case
collapses into the idle one. It prints the CPU count it saw for that reason.
`/proc/sched_prof` needs `--features sched-prof` passed to the **ISO** target,
not the kernel target, and reports cumulative work rather than a rate, so a
measurement is: read the file, run the workload, read it again, subtract.

```bash
make edos-x86_64.iso CARGO_FLAGS="--features sched-prof"
scripts/edos-vm start --smp 1
scripts/edos-vm type 'cat /proc/sched_prof > /tmp/b.txt; switchbench 20000 -l; \
    cat /proc/sched_prof > /dev/klog; cat /tmp/b.txt > /dev/klog' --enter
```

## Where things stand (2026-08-11, quiet host)

Five consecutive `switchbench` runs on a single-CPU boot, each already the best
of six batches. The median is below; the spread across the five runs is in the
last column, and it is a few percent everywhere.

| | ns | spread |
|---|---|---|
| `sched_yield`, nothing else Ready | 285 | 283-288 |
| `sched_yield`, handover to a sibling thread | 340 | 328-350 |
| `sched_yield`, handover to another process | 505 | 499-533 |
| `getpid`, i.e. a syscall that does nothing | 94 | 92-95 |
| `read` of a descriptor that does not exist | 128 | 128 |
| a pipe write + read, nothing blocking | 387 | 384-537 |
| a blocking pipe round trip between two processes | 2016 | 2009-2036 |
| the same round trip, one address space | 1808 | 1800-1812 |

The last three rows were re-taken on 2026-08-12 after the wait-queue work in
section 1 (402 / 2203 / 1988 before it); the rest have not moved.
| the switch itself, `/proc/sched_prof` | 220 | |
| a wake (`do_wake`) | 51 | |

Inside the 220 ns switch: `page` 66-77, `fxrstor` + `fxsave` 91, `CpuContext`
copies 36, publish 19, transition 27, `wake_sleepers` 18, pick 12, timer 10.

### These are the floor, and the earlier ranges were the host

The previous version of this table gave `getpid` as 96-302 ns and the pipe echo
as 530-2090, and concluded that nothing under about 30% could be attributed.
That spread was **the host**, not the code. This is a VM, and when the host has
something else to run it deschedules the whole vCPU; the guest cannot see it
happen, and it looks exactly like slow code.

With no build and no test suite running on the host, the same binary repeats to
within 2%. What the machine was doing during the run above: a permanently
resident Ethereum devnet (two `ethrex`, a `geth`, a `lighthouse`), about 2.5 of
12 hardware threads. That is this machine's floor rather than a truly idle one,
and it is quiet enough to attribute 25 ns.

So the rule is simpler than it was: **do not measure while anything is
building.** Take five runs and read the median. The desktop still interferes
from inside the guest -- the compositor wakes ~74 times a second, the panel
clock once a second -- which is what the best-of-six inside `switchbench` is
for.

`/proc/sched_prof` remains the tool for attribution *inside* a call, with one
caveat learned the hard way below: its probes cost two `rdtsc` reads per stage
boundary, and the compiler is free to move work across a boundary that is only
delimited by a non-serialising instruction. Stage numbers rank the parts of a
call; they do not add up to the call.

### What the floor says about where the time is

`getpid` is 94 ns and a bad-fd `read` is 128, so the syscall boundary is 94 and
the fd table costs 34 on top of it. A pipe write plus read is 402: two
boundaries are 188 of that, two fd lookups another 68, and roughly 145 ns is
the pipe's own work for both calls together.

`sched_yield` with nothing else Ready is 285, so the switch and its trampoline
are the other ~190 -- less than the 220 `/proc/sched_prof` reports for the
switch alone, which is the probe overhead showing.

**The syscall boundary is the biggest single term in any short call**, and
every call in the system pays it. That is item 2.

## 1. There was no gap: it was the benchmark, and ~620 ns is what is left

A blocking pipe round trip read **2203 ns**, not the ~4900 this file reported
for months. The difference was never the kernel. Every figure in this section
is that 2203 ns baseline; the round trip reads 2016 ns today, and the change
that moved it is at the end of the section.

`pipe_round_trip` timed one batch of 2000 trips with no warmup, while every other
figure in `switchbench` is the best of six batches after 64 warmup iterations. A
`fork`ed child starts with every page copy-on-write, so that single unwarmed
batch charged the round trip for the faults of the child starting up. Measured
the same way as everything else, with both ends warmed:

| | ns/round trip | spread |
|---|---|---|
| cross-process | 2203 | 2196-2206 |
| one address space (thread at the far end) | 1988 | 1983-1992 |
| **the address space, per switch** | **~108** | |

**~108 ns agrees with the yield path**, where a cross-process handover costs 129
ns more than a same-process one. Two independent measurements of the same
quantity that now match, where before they differed by a factor of eleven. That
agreement is the reason to believe these numbers and not the old ones.

### What this refutes, and it was nearly built

A `CR3` reload here does **not** cost microseconds in TLB refills, so nothing
that reduces refills is worth building. The probe that settles it is in
`switchbench` permanently: both round trips can touch a working set of 32 user
pages per side per trip, and the cost of doing so is the same whether or not an
address space was switched in between.

| | 0 pages | 32 pages | delta |
|---|---|---|---|
| one address space | 1988 | 2280 | +292 |
| cross-process | 2203 | 2590 | +387 |

So 32 extra pages per side cost ~95 ns more when a `CR3` reload sits between
them: **~1.3 ns per page**, not the ~100 that a nested walk was assumed to cost.
Huge pages for user mappings were chosen as the next piece of work on the
strength of the refill theory and would have bought nothing measurable. PCID,
which this file already records as unavailable on this host, would also have
bought nothing here.

**The measurement lesson, which is the durable part:** never compare a
best-of-N-with-warmup figure against a single unwarmed batch. The first version
of the thread-vs-process comparison did exactly that, and the ~2700 ns of
apparent address-space cost was the child's COW faults amortised over one short
batch. It survived review because it was *stable* -- an artifact that reproduces
is still an artifact.

### What is actually left: ~620 ns per round trip

Priced from parts that are all measured the same way:

| | ns |
|---|---|
| 4 pipe syscalls, at 94 boundary + 34 fd table + ~73 pipe work | 804 |
| 2 switches, at ~230 plus ~108 when the address space changes | 676 |
| 2 wakes (`do_wake`) | 102 |
| **accounted** | **~1580** |
| **measured** | **2203** |

~620 ns, or ~310 per park/wake pair, and the suspect was the predicate: the
blocking read performs a whole read attempt -- take the lock, drain nothing,
build the state -- before it blocks, and `wait_internal` then evaluated its
predicate up to three more times (at entry, after enqueueing, and inside
`transition_park_while`) with a queue push and a `retain` around them.

**Two of the three are now gone (2026-08-12), for a measured 2203 -> 2016 ns.**
`wait_until_unready` skips the entry evaluation for a caller that has just
established the condition is false under the real lock (`sys_read`'s pipe arm),
and the tail evaluation was dead for every untimed waiter: both of its branches
returned `Parked`. The enrol-then-re-check that closes the lost-wakeup window is
untouched, which is why this is safe; only checks whose answer was already known
were removed.

| | ns, median of five, 1 vCPU |
|---|---|
| cross-process blocking round trip, before | 2203 |
| after | 2016 |
| pipe echo, nothing blocks (control) | 402 -> 387 |
| `sched_yield` idle (control) | 285 -> 280 |

What is left of the ~620 is the third evaluation inside `transition_park_while`,
which is structural — it is the check that makes the park safe — plus the read
attempt itself, which a caller cannot skip without knowing the pipe is empty
before it takes the lock.

### Done: the kernel half was only global for what existed at boot (2026-08-11)

Still stands, and it is now the only measured win against the address-space
switch: **506 -> 456 ns** for a cross-process `sched_yield` handover, of which
the address-space part went **177 -> 128**.

`mark_kernel_mappings_global` is a one-time sweep of the kernel half at boot.
Anything mapped into the kernel half *after* it was not global -- including the
two regions every syscall and every switch touch: a thread's kernel stack
(`kthread_stack_alloc`, 32 KiB per thread) and the per-CPU scheduler stack the
voluntary switch pivots onto. Both died on every `CR3` write.
`MemoryManager::map_memory` now adds `GLOBAL` to any kernel-half mapping itself,
so the next site cannot forget it.

The controls are what make it credible: the thread handover (328 ns) and the
same-address-space round trip did not move, and only the cases that reload `CR3`
did. Freed kernel stacks keep their mapping (`kthread_stack_free` returns the
region to a freelist and reuse hands back the same virtual address over the same
frames), so no global entry outlives what it maps; where a kernel mapping *is*
torn down, `Mapper::unmap`'s flush is an `invlpg` and `tlb_shootdown` either
issues `invlpg` per page or toggles `CR4.PGE`, and both ignore the `G` bit.

### Retracted: the L4-style direct handoff

An earlier draft proposed an L4/seL4 direct process switch on the strength of
`(round_trip / 2) - (yield handover)`, a subtraction that charges the entire
remainder of a round trip to the wake. Measuring the wake directly refuted it:
`do_wake` is 51 ns end to end, `wake_enqueue` 32 of that, and `pick` 16. The
scheduler's share of an IPC is about 100 ns. A fastpath would remove some of
that and none of the 3 microseconds above.

If it ever does become worth it, the reference design is seL4's: the sender
switches straight to the receiver on its own timeslice, no runqueue and no
scheduler invocation, behind a fastpath requiring the receiver to be runnable
here with nothing higher-priority waiting.
<https://docs.sel4.systems/Tutorials/ipc.html>,
<https://microkerneldude.org/2019/03/07/how-to-and-how-not-to-use-sel4-ipc/>

### Done: the pipe and PTY data path (2026-08-11)

A pipe write plus read, nothing blocking, went **480 -> 402 ns**, and the same
change removed an allocation per keystroke from the PTY. What it was:

- `Pipe::read` allocated a `Vec` for the bytes it drained and then `drain(..n)`
  memmoved everything left behind. Both sides of a PTY did the same.
- `sys_write` allocated a `Vec` to stage the user's bytes before taking the
  pipe lock, on every call.
- `sys_read` took the pipe lock once to clone `reader_wq`, again to drain, and
  again inside every `wait_until` predicate evaluation.

Now: one `ByteRing` (`kernel/src/util/ring.rs`) behind the pipe and both
directions of the PTY, which grows to fit and keeps its allocation, so two
processes passing single bytes settle on one buffer and never allocate again.
Reads fill a caller-provided buffer, so the device never allocates on a read.
The non-blocking path takes the lock once; `reader_wq` is only fetched when the
read actually has to park.

**The trap, and it is worth remembering because it hid the whole win.** The
staging buffer that replaced the per-call `Vec` was a stack array, and Rust
zeroes a stack array — so the change swapped an allocation for a memset of the
array's full size, whatever the transfer. At 2048 bytes the two cancelled
*exactly*: 480 ns before, 480 after. The measured curve, one-byte echo:

| staging buffer | ns |
|---|---|
| a `Vec` per call, as before | 480 |
| 2048 B | 480 |
| 512 B | 455 |
| 128 B | 404 |

128 B is what shipped: enough for a byte of IPC or a keystroke, with anything
larger taking the heap, where one allocation is amortised over a copy worth
making. The general lesson is that **a stack buffer is not free, it costs its
declared size**, and the cost does not appear in a profile taken with
`sched_prof` — the compiler moved the memset across the probe boundaries, which
is how `pipe_copy_out` came to read 135 ns for copying one byte.

### Done: a wake that costs nothing when nobody is waiting (2026-08-12)

`notify_pollers()` ran on every read and every write, cloned the reader's
`WaitQueue` `Arc`, and called `wake_one()`, which disabled interrupts and took a
spin lock only to find the queue empty. `WaitQueue` now carries a `waiters`
count, written to `inner.len()` under the queue lock, and `has_waiters()` reads
it with no lock at all; `wake_one`/`wake_all` return early on it, so every one
of the ~60 wake sites in the kernel gets the same skip.

The ordering is the whole of it, and a barrier supplies it rather than an
ordering annotation. `has_waiters` opens with a `SeqCst` **fence**, because a
`SeqCst` load alone is a bare `mov` on x86 and would leave the producer's
publication sitting in the store buffer while the count is read. That is the
store-buffer litmus with one side fenced, which is a lost wakeup. The lock this
check replaced was the barrier before, and the fence stands in for it.
`doc/WORKING-NOTES.md` has the codegen and the two producers it would otherwise
have hung.

With the fence in place:

- a waiter publishes its enrolment **before** re-checking its predicate (the
  store is inside the enqueue's critical section, and the re-check follows the
  lock release);
- a producer's publication is ordered **before** its read of the count, whatever
  that publication was: a lock release, a `compare_exchange`, or a plain
  `Release` store.

So the two cannot both miss each other, and the guarantee holds for all ~60 wake
sites rather than only the ones whose producer happened to publish under a lock.
The count is exact rather than a hint (every mutation of the deque republishes
its length), so over-counting can only cost a wake on an empty queue, which is
what happened unconditionally before.

### Known gap: a pipe has no backpressure

`Pipe::write` never blocks and the ring grows without limit, so a writer that
outruns its reader grows the kernel heap until it dies. The only thing that
stops it today is `readers == 0`, which is what makes `yes | head -1` terminate.
POSIX wants a bounded pipe whose write blocks when full.

Deliberately not done with the ring, because it is a semantic change with a
deadlock attached: `edos-sh` writes a whole heredoc into a pipe *before*
anything reads it (`main.rs`, `script.rs`), so a heredoc larger than the
capacity would deadlock the shell against itself. The real fix is both halves
together — a capacity plus a writer wait queue in the kernel, and a shell that
does not write into a pipe it has not yet given anyone to read.

## 2. What every syscall does before it reaches the syscall

`getpid` is 94 ns and it is the shortest call the kernel has: the SYSCALL entry
stub, the dispatch, a read of the current thread, and SYSRET. Every call in the
system pays it, so it outranks anything that belongs to one subsystem.

What sits on that path and has never been read with this in mind: the
traced-call bookkeeping in `syscall_handler` (`with_current_thread` and
`trace::traced_session` run on *every* call, whether or not anything is being
traced), and `current_thread_info()` plus `info.lock()`, which the arms
themselves then call again.

The fd table costs a further 34 ns on top, measured as the gap between `getpid`
and a `read` of a descriptor that does not exist — that one is a
`BlockingMutex` acquisition and a lookup, before any descriptor is cloned.

Reference for how small this can get: Linux's `SYSCALL`/`SYSRET` path with no
mitigations is on the order of 40-60 ns on comparable hardware.

## 3. A voluntary switch does not need a register frame or an `iretq`

`sched_yield` with nothing else Ready is 285 ns and a `getpid` is 94, so the
switch and its trampoline are the other **~190**. (`/proc/sched_prof` says 220
for the switch alone, which is more than the whole difference: its probes cost
two `rdtsc` per stage boundary. Read the stages as a ranking of the parts, not
as a total.) That 190 buys: `save_transition_switch` builds a synthetic
160-byte `CpuContext` on the stack, `do_save_current_thread` copies it into the thread under a spin
`Mutex`, `context_switch_to` copies the next thread's back out under another,
and the trampoline `iretq`s **kernel-to-kernel** into a resume label that then
`ret`s to the caller.

Almost none of that is needed on this path. A voluntary switch happens at a
function call boundary, and the C ABI already permits a call to clobber
`rax rcx rdx rsi rdi r8-r11` — so nine of the fifteen GPRs are being saved for
nobody. Linux's `__switch_to_asm` saves `rbp rbx r12-r15`, swaps `RSP`, and
`ret`s; there is no interrupt frame and no `iretq` anywhere in it.

The work: give the voluntary path its own saved form (six registers and a
stack pointer) instead of sharing `CpuContext` with the preemption path, which
genuinely does need the full frame because it resumes an arbitrary
instruction. The two paths stop sharing one representation, which is the part
to design carefully — `Thread::ctx` is read by work-stealing and by
`validate_ctx`, and both need to know which kind they are looking at.

**Re-measured 2026-08-14, and this item is smaller than it reads.** The "36 ns
of `CpuContext` copies" is two stages that price at 23.9 and 8.4 ns/call, of
which ~8 is each stage's own probe — so the copies are ~16 ns, and the save side
is free. The FPU pair in the same table is ~74. Skipping the reload when the
registers already hold the incoming thread took `sched_yield` from 292 to 252 ns
without touching any of this machinery; see the section on it below.

What is left for this item is the `iretq`, the frame build and nine register
saves nobody reads — real, unmeasured, and against the highest risk in the tree,
since `Thread::ctx` is read by work-stealing and `validate_ctx`. Price the
`iretq` on its own before starting: if a kernel-to-kernel `iretq` is ~30 ns
here, the whole item is ~45, which does not obviously justify two saved forms.

Reference: <https://kernel-internals.org/sched/context-switch/>

## 4. Spin briefly before parking — not yet justified, measure first

`BlockingMutex` and the wait queues park immediately on contention. Spinning
first for about the cost of a switch turns a short wait into no switch at all,
and LWN reports a futex microbenchmark going from 35M to 54M operations with
adaptive spinning. <https://lwn.net/Articles/386536/>

**But nothing here has measured a wait short enough for it to help.** On the
single-CPU boot every measurement in this doc was taken on, spinning cannot
help by construction: the thread that would satisfy the wait cannot run while
the waiter spins, so every spun cycle is pure waste before the park happens
anyway. It pays only when the holder is running *concurrently* on another CPU,
and there is no multi-CPU contention measurement here to size it against.

So the prerequisite is a benchmark that holds a `BlockingMutex` across a short
critical section from several CPUs and reports the wait distribution. If the
common wait is shorter than a switch, this becomes a real item; if it is
longer, adaptive spinning is a pessimisation and should be recorded as refuted.

When it is built: derive the bound from the measured switch cost rather than
picking one, and give up the spin if the holder is not currently running —
that check is what a pure spinlock cannot make and is why it is called
*adaptive*.

## 5. `MIN_TIMER_INTERVAL` is still a picked number

The 10 us floor in `apic/mod.rs` was chosen to sit above interrupt cost and
below a 1 ms sleep, not measured. It matters much less now that the one-shot
is re-armed rarely. TSC-deadline mode would remove the need for a floor at all
(an absolute deadline rather than a counted-down interval) and is the other
reason to consider it; it is **not** a way to make arming cheaper, since it is
still a trapping MSR write.

## What has been tried and did not work

- **Mirroring the thread's `CR3` to keep `switch_to_page` off a lock.** Shipped,
  because the lock is a hazard worth removing -- `execve` holds `user.write()`
  while it installs an image, and a switch to a sibling thread of that process
  would spin behind that guard *inside the switch* -- but it is **not** worth
  measurable time, and the item claiming 66-77 ns was wrong. `sched_yield` with
  nothing else Ready: 285 -> 283 ns. A handover to a sibling thread: 328 -> 327.
  To another process: 500 -> 506. All inside the noise. The 66-77 ns came from
  the `page` stage of `/proc/sched_prof`, which is two `rdtsc` reads wide before
  it measures anything; a `PreemptRwLock` read acquire and release is 10-20 ns,
  and 10-20 ns is not visible against a 285 ns call at a 2% noise floor.

- **`XSAVEOPT` instead of `FXSAVE`.** Measured: save 32 → 36 ns, restore
  59 → 83 ns. It can only win by *skipping* components, and with `XCR0`
  holding x87 and SSE there are none — it covers the same registers `FXSAVE`
  does and adds a 64-byte header plus per-component work. Its modified
  optimisation needs consecutive `XSAVEOPT`/`XRSTOR` against the same area,
  which two threads handing off to each other never produce. It becomes right
  only if `XCR0` grows something large and optional (AVX and wider) that most
  threads leave alone. The note lives above `save_fpu_state`.

- **PCID.** Not possible on this machine, and not because it is old — Intel
  has had it since Westmere in 2010. AMD did not ship it for a decade, and this
  host, a Ryzen 5 5600 (Zen 3, Vermeer), does not expose it: no `pcid` in
  `/proc/cpuinfo` (`invpcid` is there, which makes a substring grep lie),
  nothing from `lscpu`, and `qemu -cpu qemu64,enforce,+pcid` refuses with
  "host doesn't support requested feature: CPUID.01H:ECX.pcid". Reporting at
  the time had Zen 3 adding it on the EPYC parts. **Check the CPU before
  planning around it.** Marking kernel mappings `GLOBAL` took the part of the
  same win that is reachable without it.

## Balance is measured on more than one CPU

The single-CPU rule at the top of this file is about the switch and wake paths.
It is exactly wrong for placement and rebalancing, where a second CPU is the
thing under test, and `switchbench` is the wrong instrument for the same reason.

`programs/balancebench` is the instrument. It is a straggler test: one worker
per CPU bar one, each doing an identical lump of arithmetic, with eight blocked
threads per CPU as ballast. One worker alone establishes what the lump costs
with a CPU to itself, so `slowest / solo` is the whole report — 1.00 means every
worker had a CPU, 2.00 means two shared one while another stood empty. It leaves
a CPU spare deliberately: the desktop is not idle, and a spare CPU is also what
makes a bad placement visible. Read `/proc/sched` (per-CPU
`CURRENT QUEUED LOAD STEALS`) beside it, which is what placement believed.

`balancebench wake` prices the other half. The straggler test above *spawns* its
workers, and a spawn already goes to the least-loaded CPU through
`pick_sched_for`; a **wake** deliberately does not, because `complete_wake`
enqueues on the waker's CPU for cache locality. So it parks a worker per spare
CPU, lets the machine go fully idle, and times a burst of wakes from one thread.
Everything that spreads that burst is work-stealing, which makes the mode a
direct measure of how fast an idle CPU learns there is something to steal.
`median burst / solo` is its report on the same scale.

**A benchmark whose inner call is pure must break loop invariance itself.**
`work` is a pure function of `(rounds, seed)` and `#[inline(never)]` does not
stop LLVM hoisting a pure call whose arguments never change clean out of the
loop around it. The first `wake` mode passed a fixed seed every round: the
arithmetic ran **once**, every round after it was a bare pipe round trip, and
the burst measured 0.02 ms against a 4 ms lump — a 140× speedup that read
exactly like a scheduler that had nothing to fix. Each round seeds the next now.
The tell was the ratio being *impossibly good* rather than bad, and the thing
that caught it was making the worker report its own loop time so the two ends of
the burst could be compared.

### Done: load is runnable work, not membership (2026-08-14)

`thread_count` counted the threads that called a CPU home and was adjusted only
on spawn, steal and exit — never on park, sleep or block — so a thread that
would not run again weighed as much as one spinning flat out, and both
`pick_sched` and `try_rebalance` balanced that. `Scheduler::load` is now the
runqueue's length plus the thread running now, republished from the queue itself
by the two helpers that own every access to it, so a parked thread is in no term
of it. The steal paths kept no counts at all afterwards.

**What it is worth, measured** — `balancebench`, 4-CPU boot, three runs each,
same host, the two builds either side of the change:

| | imbalance | wall for the same work |
|---|---|---|
| membership (before) | 1.75, 1.93, 1.94 | 273, 299, 300 ms |
| runnable load (after) | 0.99, 0.98, 0.99 | 150, 150, 151 ms |

Three workers, four CPUs, 32 blocked threads in the machine. Before the change
two of the three workers landed on one CPU and took 294 ms against a 152 ms
solo, while a CPU stood empty; after it, each finished in 149 ms. **Twice the
throughput on this workload**, and the reason is that the blocked threads were
being counted as work.

It costs nothing on the switch path. `switchbench`, single-CPU boot, five runs
each side, medians: yield idle 308 → 304 ns, yield thread 304 → 303, yield
process 418 → 417, `getpid` 94 → 94, bad-fd read 169 → 169, pipe echo 451 →
450, cross-process round trip 2255 → 2266, one address space 2044 → 2029. Every
delta is inside its own run-to-run spread, so the `total_len()` sum and the
atomic store `with_rq` adds are invisible against a 300 ns yield. Tracking the
length inside `RunQueue` instead would buy nothing.

Watched fail first: 32 threads pinned to one CPU and parked there took **0 of
16** placements before the change. The gate is the `load-parked-is-not-load`
sched-test case; `doc/WORKING-NOTES.md` has why its first form was flaky green
and what replaced it.

Still open in the same area, and now separate defects rather than one: parked
threads never migrate, so a thread wakes back onto the CPU it parked on; and
`REBALANCE_THRESHOLD = 2` and `REBALANCE_INTERVAL = 10` are picked numbers that
nothing has measured against the new quantity.

### Done: the current thread's info is cached per CPU (2026-08-14)

`current_thread_info()` was a registry lookup every time it was called —
`without_interrupts`, a `RwLock` read over `THREADS.infos`, a `BTreeMap` get and
an `Arc` clone — and a syscall calls it several times over: once for the errno it
clears on entry, once per arm that wants the fd table or the working directory,
and once more on the way out if the call failed. `sys_getpid` is *only* that
lookup plus a lock, which is why the shortest call the kernel has cost 94 ns.

This CPU's `PerCpuData` now holds the running thread's `UserThreadInfo` beside
the thread itself, keyed by thread id, filled on the first call of a turn and
dropped by `set_current_thread` on the way out. Measured on a single-CPU boot,
five runs, medians:

| | before | after |
|---|---|---|
| `getpid` | 94 ns | 85 ns |
| `read` of a bad fd | 169 ns | 145 ns |
| pipe echo, nothing blocks | 450 ns | 425 ns |

Every syscall in the system is ~9 ns cheaper, and one that touches a descriptor
or fails is ~25 ns cheaper because it made more than one call.

What makes the cache sound rather than merely fast: a live thread's registry
entry is never replaced (both `insert_thread_info` sites are creating a *child*,
and `execve` mutates the existing info through its lock rather than swapping the
`Arc`), thread ids are never reused so a key match is a real match, and the fill
runs with interrupts off so the thread cannot migrate between the read and the
store. Dropping the entry on switch is what keeps a dead thread's fd table and
address space from being held alive by a CPU that has moved on.

### Done: a wake no longer looks its target's CPU up in the registry (2026-08-14)

`mark_running_thread_need_resched` ran on every `enqueue_ready`, every sleeper
woken, every `spawn_thread` and every steal, and reached the thread to mark
through `get_thread_by_id` — an `RwLock` read over `THREADS`, a `BTreeMap` walk
and an `Arc` clone. The CPU being marked is almost always the one running the
code, because `complete_wake` enqueues on the waker's CPU for locality, so the
thread was already in that CPU's own slot. It reads the slot now and keeps the
lookup only for a genuinely remote CPU, which every caller follows with an IPI
anyway.

| | before | after |
|---|---|---|
| pipe round trip, cross-process | 2206 ns | 2184 ns |
| pipe round trip, one address space | 2006 ns | 1966 ns |
| pipe echo, nothing blocks (control) | 425 ns | 424 ns |
| `getpid` (control) | 85 ns | 83 ns |

Two wakes per round trip, so ~11–20 ns off each against a `do_wake` measured at
51 ns. The two controls take no wake at all and did not move, which is what
makes the rest attributable.

### Done: an enqueue wakes a halted CPU instead of waiting for its poll (2026-08-15)

`complete_wake` enqueues on the **waker's** CPU for cache locality, so a thread
that wakes several others buries them all in one runqueue however much of the
machine is asleep. Spreading them is work-stealing's job alone — and an idle CPU
learned there was anything to steal only when its own backoff poll came round.
`run_idle` halts for up to 100 ms at a time when it has no sleeper of its own,
and polls at ticks 0, 2, 4, 8, 16, then every sixteenth, so the tail of that
backoff is *seconds* of a runnable thread sitting in a queue beside seven idle
CPUs.

An enqueue that leaves two or more threads queued now claims a CPU out of
`IDLE_CPU_MASK` and sends it a reschedule IPI. Three things make it small:

- **The claim is the message.** Clearing the bit is the only thing that clears
  another CPU's bit, so the woken CPU reads "I was asked for" out of finding its
  own bit gone — no second flag, and no registry lookup to set one. It also
  stops two enqueues in a row from both shouting at the same CPU while others
  sleep.
- **The threshold is `try_steal`'s own rule, read from the other side.** That
  function refuses to take a CPU's only queued thread, so a queue of one has
  nothing to offer and poking anybody for it only spends a wakeup.
- **The bit is published across the halt and taken back on the way out**, so it
  means *halted* rather than *idle*. `sti; hlt` is one unit against a claim that
  lands in between: an IPI raised while interrupts were off is delivered after
  the `hlt` begins, not before it, so the wakeup cannot be missed.

The poll survives as the backstop for a claim that raced, not as the mechanism.
A CPU that was poked and found nothing restarts its backoff, because being asked
is evidence the machine has work and the next ask must not land in the middle of
a sixteen-tick sleep.

**What it is worth, measured** — `balancebench wake`, 8-CPU boot, 7 workers,
median of 10 bursts, same host and the same userspace binary either side:

| | fanout (median burst / solo) |
|---|---|
| poll only (before) | 4.25, 4.44 |
| poked (after) | 1.97, 2.02, 2.03, 2.00 |

Watched fail first, by deleting the one `poke_idle_cpu()` call and rebuilding:
17.47 ms median burst against 8.39 ms with it. The worker-side number is what
says *why*: before the change a worker's own loop took 4.3–8.2 ms against a
4.1 ms solo, because workers were sharing CPUs; after it every worker reports
~4.3 ms, so each had a CPU to itself. `/proc/sched` agrees from the other end —
steals went from 44 on a single CPU and 0–2 everywhere else to 8/11/13/11 spread
across four.

It costs nothing on the switch path. `switchbench`, single-CPU boot, against the
2026-08-14 figures: `getpid` 85 → 81 ns, bad-fd read 145 → 146, pipe echo 425 →
422, yield idle 252 → 255, yield process 417 → 420. Every delta is inside the
run-to-run spread, which is what the threshold buys: a busy machine ends the
whole thing on one relaxed load of a zero mask.

**Still open in the same area.** The residual 2.0 is one CPU running two lumps
back to back, not seven CPUs each running one: by the last wake of a burst the
queue has already drained below the threshold and that wake pokes nobody. Whether
to spend a wakeup there is a real question and this leaves it conservative.
`tick_finish` re-enqueues a preempted thread without poking either, and
`spawn_thread` still only IPIs the CPU it chose. Both are the same one-line
extension; neither is measured.

### The table above is stale in two rows, and one of them was a regression

Re-measuring the whole of `switchbench` on 2026-08-14 against a comparably quiet
host (the same resident devnet, load average 1.4) found two figures that have
moved since 2026-08-11, in **both** builds either side of the load change, so
neither is that change:

| | 2026-08-11 | 2026-08-14 | after the cache |
|---|---|---|---|
| `read` of a bad fd | 128 ns | 169 ns | 145 ns |
| pipe echo, nothing blocks | 387 ns | 450 ns | 425 ns |
| cross-process round trip | 2016 ns | 2266 ns | |
| `getpid` | 94 ns | 94 ns | 85 ns |

`getpid` was unchanged, so the syscall boundary itself did not move; what moved
was what happens on top of it, and most sharply on the **error** return.

**Bisected, and it was the negative-errno conversion.** Building the tree at
whole commits either side, single-CPU boot, three runs each: the bad-fd read is
140–141 ns at `4c0cffc` and 169 at `6fbb047`, so that one commit costs **+29 ns
on every failing syscall**. The mechanism is the line it added to
`syscall_handler` — `current_thread_info().lock().errno` — which was a second
registry lookup on the way out. The per-CPU cache above takes it back without
giving up the feature, and the same bisect priced the rest of the window:

| | bad fd | pipe echo |
|---|---|---|
| 2026-08-11 (recorded) | 128 | 387 |
| `61a3dec`, end of 08-13 | 144 | 452 |
| `4c0cffc`, before negative errno | 140 | 450 |
| `6fbb047`, negative errno | 169 | 450 |
| now, with the info cache | 145 | 425 |

So the error path is recovered, and **~38 ns of the pipe echo is still
unaccounted for**: it arrived between the 2026-08-11 reading and `61a3dec`, it
is on the *successful* fd path rather than the error one, and the only two
commits in that window that touch the fd or pipe code are `eb4dd2f` (named
pipes, the bounded PTY) and `f96e896` (`O_NONBLOCK` that outlives the open,
which added a second table lookup per read and write). Bisecting those two is
the next step in this thread; the harness is in `doc/WORKING-NOTES.md`.

`yield thread` also reads 1–3 ns over idle now against 55 ns then, on both
builds. That one is probably not a regression but a correction: the idle case
already performs a full save and restore, so handing over to a sibling in the
same address space should cost about the same, and the 2026-08-11 figure was
taken when the APIC one-shot was re-armed on every switch.

## Recently closed

The 2026-08-11 round, in `doc/WORKING-NOTES.md`: the APIC one-shot is re-armed
only when what is armed would fire too late (that write alone was 1024 ns of a
1270 ns switch), `FS.base` goes through `rdfsbase`/`wrfsbase`, and kernel
mappings are `GLOBAL` with `CR4.PGE` on. `sched_yield` 1917 → 433 ns as
measured then, and 285 when re-measured on a quiet host.

**The intermittent `sched-test` ping-pong failure was a test bug, not a lost
wakeup.** It had been open since 2026-08-10 under two signatures — `ping-pong
count mismatch: 499 != 500`, and a timeout where `ping-pong-ping` passed and
`ping-pong-pong` never reported — and both were read as a late or lost wake on
the pong side. Neither was.

`thread_park` may return **without a matching wake**: a `wake_pending` token
published while the thread was still running is consumed by the park
transition and short-circuits it. `thread_park_while`'s doc says so and states
the rule — callers must loop on the actual condition. The test did not: it
paired 500 bare parks against 500 wakes and treated a park as a receipt for
one wake. One spurious return put the two sides a round out of step, and from
there both signatures follow. Ping reaching its last park early lands in the
window where pong has woken it but not yet counted the round, which is the
`499`; ping instead consuming a later round's wake and leaving the loop means
pong waits for a wake that never comes, which is the timeout. A second defect
widened the first: pong woke ping *before* incrementing the counter.

Fixed by driving the handshake off a turn variable each side loops on, and by
counting before handing the turn over. Failed about 1 run in 8 before; 33
consecutive clean runs after.

The lesson generalises past this test: **`thread_park` is not a counting
semaphore**, and any caller pairing parks against wakes one for one has the
same latent bug. It is worth grepping for bare `thread_park()` outside a
condition loop.

## Measure on a quiet host, and check rather than assume

The rule at the top of this file said "do not measure while anything is
building". On 2026-08-14 that was not enough: a DDNet build and its test run
took ~7 of this machine's 12 threads in bursts of about a minute, twice landing
in the middle of a five-run measurement. The signature is unmistakable once
seen — the *minimum* of the runs matches or beats the baseline while the maximum
is 30% worse, because interference only ever adds time:

```
yield idle       244 300 286 318 299      <- one clean run, four contaminated
round trip     2184 ... 3007              <- baseline was 2184
```

`switchbench` already takes the best of six batches *inside* a run, so
cross-run spread that wide is always the host.

**The harness samples the host now.** Percent of the machine busy over one
second from `/proc/stat`, taken before the first run and around every run
after it: it refuses to start above 40%, and flags any individual run that went
busy. The one-minute load average is the wrong instrument — it lags a burst by
minutes in *both* directions, so it misses one starting and then refuses to
measure for minutes after one ends.

### Done: the FPU reload is skipped when the registers already hold it (2026-08-14)

`fxsave` and `fxrstor` were the largest single item in a switch — ~74 ns of a
292 ns yield between them, three times what the `CpuContext` copies cost. The
restore is now skipped when the CPU's registers already hold the incoming
thread's state, which is what happens on a self-switch (a yield with nothing
else Ready) and on user → kernel thread → the same user thread, the shape every
interrupt-driven wake takes.

**What makes it sound is the target spec**: `x86_64-unknown-none` is built
`-sse,-sse2,…,+soft-float`, so kernel code cannot touch an FPU register and a
user thread's state survives arbitrary kernel execution. The only thing that
overwrites those registers is another restore here.

The claim has two halves and needs both. This CPU's `fpu_owner` alone would miss
the thread having run on another CPU and changed its registers there; the
thread's `fpu_cpu` alone would miss this CPU having loaded somebody else's state
since. The save stays eager, which is what keeps a migrated thread's saved area
current — deferring it would let a stealer restore from an area the previous CPU
never wrote.

| | before | after |
|---|---|---|
| `sched_yield`, nothing else Ready | 292 ns | 252 ns |
| `sched_yield`, handover to a sibling | 300 ns | 310 ns |
| everything else in `switchbench` | | unchanged |

The sibling handover alternates ownership every switch, so it cannot skip and
does not move; the round trips and the syscall floor are unaffected.

**The gate is `programs/fputest`**, which nothing in the tree had: four threads
seed `xmm0`-`xmm7` with distinct patterns and read them back across 20,000
yields each, with the load, the syscall and the read-back in one `asm!` block so
the compiler cannot use an `XMM` register in between and hide a failure. Watched
fail: with the restore skipped unconditionally, all four threads report their
lanes back as zero at round 0.

**What it does not cover, stated plainly:** dropping the `fpu_cpu` half — the
migration race — still passes, at 4 threads and at 16 threads on a 4-CPU boot.
That case needs a CPU to keep its claim while the thread runs elsewhere and
comes back, which wants few threads and idle CPUs rather than many, and no
arrangement tried here reproduced it. The second half of the check rests on
reading, not on a red test.

## Refuted: two of three switch optimisations bought nothing

Sized from `/proc/sched_prof` stage numbers and then measured, which is the
right order and produced two negatives worth recording:

- **Caching the active `CR3` per CPU** instead of reading `CR3` to compare.
  The `page` stage reads 25.8 ns/call on a switch that does not change address
  space, which is nothing but a `Cr3::read` and a compare — so it looked like
  ~18 ns of real cost. Measured: no change outside noise on any of the eight
  figures. `Cr3::read` is cheap, and that stage number is mostly its own probe.
  Not kept: it made `write_cr3` the only permitted writer of `CR3` in the
  kernel, which is a real invariant to maintain for a win that does not exist.
- **`wake_sleepers` returning early on the atomic `earliest_deadline`** instead
  of taking the sleepers lock to find an empty heap. Also no measurable change.

The lesson is the one this file already records about the `page` stage and then
failed to apply: **a stage number is one `rdtsc` wide before it measures
anything, so it ranks the parts of a call and cannot size one.** Anything under
about 20 ns/call in that table is indistinguishable from its own probe.
