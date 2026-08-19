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

## 6. Done: priority inversion, and the residual was the instrument (2026-08-18)

The Mars Pathfinder shape, and `prio-inversion` in `thread/sched_test.rs` is
the instrument: on one CPU, a holder at `DEFAULT_PRIORITY` needs 10 ms of CPU
inside the section, a hog five levels above it stays runnable, and a waiter at
priority 15 — the highest the machine has — blocks on the same lock.

Measured on a 4-CPU boot, before any lending:

```
prio-inversion: the top-priority waiter blocked 181776 us on a 10000 us section
(18.17x), holder stretched 18.37x by a hog 5 levels above it
```

The two numbers being nearly equal is the mechanism stated plainly: **the
waiter's wait is the holder's stretch.** Every microsecond the hog takes from
the holder is a microsecond added to the wait of a thread eight levels above
the hog, and the only thing bounding it is how long the hog chooses to run.

`BlockingMutex` lends: a waiter raises the holder to its own effective priority
for the length of the section (`Thread::lent_priority`, and every `weight_of`
site reads `effective_priority()`). That took the factor to 7.67x, and this
section used to end there, calling the remaining 7.67x an open gap against the
1.5x that pure share predicts, with two candidates for it.

**Candidate 1 was the whole of it, and it was worse than stated.** The entry
said the other cases "are not pinned away from it". They are not, but the fatal
one is not a pinning question at all: `test_starvation_spinner` puts a
**priority-13 spinner on every CPU for 400 ms**, so no pin could have escaped
it, which is also why a single-CPU boot read *worse* rather than cleaner and
why that reading was rightly not trusted. On top of it `contend_cpu` — which is
where this case ran — carried `share-heavy`, a flat-out spinner at priority 14,
one level below the waiter. Most of the stretch being attributed to the hog was
those two.

Given a CPU of its own and a machine that has stopped saturating itself, the
same instrument at the same commit reads:

```
prio-inversion:                the waiter blocked 1.40x, holder stretched 1.50x
prio-inversion-rwlock-write:   the waiter blocked 1.40x, holder stretched 1.50x
prio-inversion-rwlock-read:    the waiter blocked 1.40x, holder stretched 1.50x
```

1.50x against the 1.51x the weight table asks for. There is no residual.
**Candidate 2 — that the loan raises the weight but does not re-place the
holder, so the `vdeadline` it already holds is stale until spent — is therefore
refuted as an explanation of anything measurable here.** It remains true of the
code, and at a section length where one stale deadline mattered it would show;
at 10 ms it does not.

### It is a gate now, not only an instrument

The case used to assert only that the hog preempted the holder, with a
hand-picked `stretch > 1.5x`. That number was calibrated against the confounded
world and went red the moment the confounds were removed, which is what found
this. Both bounds are derived from the weight table now:

- the holder shares its CPU with the hog in proportion to weight, so a section
  costing `c` of CPU takes `c * (w_holder + w_hog) / w_holder` of wall clock;
- **lent**, the holder is served at the waiter's weight: `(6104 + 3125) / 6104`
  = 1.51x;
- **unlent**, at its own: `(1024 + 3125) / 1024` = 4.05x.

The gate is the midpoint. A holder that inherits nothing lands above it, which
was watched: deleting the lend from `RwLock` put the write-held case at 3.52x
and the read-held case at 3.53x, each against a 2.78x gate, while the other
flavours stayed green — so the gate is specific to the lock it names.

### What lends now, and what a set of holders costs

- `BlockingMutex` (76384ed): one holder, named in the lock, loan ended by the
  release.
- `RwLock`, both directions: the write holder is published the same way. Read
  holders are a *set*, so they are recorded in a slot table on the lock and the
  loan is ended by the **waiter** rather than the release — a release has no
  single holder to name, and one that tried to end loans owed to other readers
  would be ending live ones.
- Recording readers is **armed**: it costs one relaxed load until the first
  writer actually blocks on that lock, because otherwise every `SCHEDULERS`
  read on the placement path and every VFS mount-table read would pay for a
  case most of these locks never see. Arming is edge-triggered, so the writer
  that arms a lock misses the readers already in flight and every writer after
  it is covered. The read-held case measures the second round for that reason.
- The futex path is different in kind and is covered in section 6a.

Known limits, unchanged: the loan is a single priority per thread rather than a
stack, so a thread holding two locks forfeits an outer loan when it releases the
inner one — conservative, never a loan that outlives its section. And the
donation reaches the holder alone; a chain of holders each blocked on the next
carries it one link per acquisition rather than transitively in one step.

### `make test-single` passes now, and two things had to be true for it

The entry above recorded that it "cannot pass". Both reasons are fixed:

1. `load-parked-is-not-load` asserted an **absolute** load below the parker
   count, and on a single-CPU boot every one of the suite's threads is on that
   CPU: it read load 51 against a bound of 32 and failed a working scheduler.
   The claim is about what parking threads *adds*, so it samples the load before
   and after and bounds the difference.
2. Nine inversion threads waiting for a quiet machine were themselves nine
   sleepers on a poll loop, and `burst-share` compares a **sleeper** against a
   steady thread. It went red at 1.42x from that alone. A single gate thread
   waits and then spawns the nine, so they do not exist while anything else is
   being measured; `burst-share` went back to 0.99x, which is the attribution.

The suite is 58 cases and passes on `make test` and on
`make test-single AUDIODEV=none`.

### The measurement cases run one at a time (2026-08-19)

Six cases say how much CPU a thread was given: the load metric, priority
starvation, three occupied levels, weighted share, lag across a sleep, and the
inversion. Each is a statement about a runqueue carrying its own threads and
nothing else, and each held its CPU for a fixed window during which the others
were running too. They got their separation from **pinning** — `contend_cpu` is
the first registered CPU, `burst_cpu` the second, `pi_cpu` the third — which
works only while there are CPUs to spread across. On a single-CPU boot all
three are CPU 0, the starvation spinner is on every CPU by construction, and
the six measured each other.

`burst-share` showed it first, because its pair is the lowest priority of the
group and its sleeper's sample is **whole slices of CPU**. On one CPU the pair
was served 2 to 3 ms of its 300 ms window — two or three bursts — against 36 to
50 ms on four. A ratio built from three samples moves in steps of a third, so
it cleared its gate about one run in four while the same build on four CPUs
read 0.86x to 0.98x every time.

A gate thread now runs the six in sequence. The cost is the sum of their
windows rather than the longest, about 1.1 s of guest time, and every one of
them got sharper for it: `prio-inversion` reads a 1.50x stretch on every run
against the 1.51x the weight table predicts, `weighted-share` 4.74x to 4.86x
against the 4.77x it asks for, and `burst-share` 1.00x to 1.01x with a spread
under half a percent. Before, on a single-CPU boot, those read 3.02x to 3.12x,
5.83x and anywhere from 0.64x to 1.36x.

### `burst-share` was gating the wrong direction (2026-08-19)

The case was built on the claim that its sleeper "leaves the runnable set at
the point it is furthest ahead — it has just spent a full slice while its
competitor waited". **It does not.** The burst is charged in the sleeper's own
CPU time, so reaching a full slice of it takes about two slices of wall clock
on a CPU it shares, during which the steady thread is served just as much, and
then the sleep hands the steady thread a further 100 us. Instrumenting
`RunQueue::record_lag` at the moment the sleeper stops being runnable: the
first two cycles read a clamped `+994866` and `+1000000` of credit and the
steady state settles between `+1275` and `+25485` — positive throughout, and
`lag = V - vruntime` is positive for *under*-served. There is no overrun to be
forgiven.

So carrying the lag is what pays the sleeper back its own sleep, and the arm
this case can prove is the opposite of the one it named. Rebuilt with `place`
reverted to placing level at `V`, the ratio goes **down**: 0.73x to 0.91x over
twelve runs, against 1.00x to 1.01x with the lag carried. The old
`ratio < 1.10` bound sits above both arms and separates nothing — verified on
the pre-serialization suite too, where the defect read 0.63x to 0.72x on four
CPUs rather than the 1.20x to 1.72x its calibration recorded. The gate is now
two-sided, and the lower bound at 0.95x is the one that goes red.

## 6a. Done: the futex path lends too (2026-08-18)

`BlockingRwLock` and `BlockingMutex` are kernel locks. The one a *program*
blocks on is the futex, and it is different in kind: **a futex word is opaque to
the kernel.** It is a `u32` in the program's own memory with no convention
imposed on it, so unlike every in-kernel lock there is no owner to read out of
it and nothing to lend to. `std::sync::Mutex` on this target is a three-state
word — unlocked, locked, contended — which never names a holder at all.

So the waiter names one: `SYS_FUTEX_WAIT_PI` (317) takes an `owner_tid`
alongside the word, and lends the caller's priority to it for the duration of
the wait. Three consequences worth stating rather than discovering:

- **The loan ends with the wait, not with the release**, because the release is
  a userspace store the kernel never sees. A waiter woken while the owner still
  holds the word re-lends on its next call, which is the loop every futex waiter
  already runs against a spurious wake.
- **A wrong or hostile `owner_tid` is bounded rather than checked.** The kernel
  cannot prove ownership of a word it does not interpret. What it can do is lend
  only the *caller's own* effective priority, and only while the caller is
  itself waiting — so the worst a lie achieves is raising another thread to the
  liar's own priority for the length of the liar's own wait.
- **A separate syscall number, not a fourth argument on `SYS_FUTEX_WAIT`.** A
  caller built against the three-argument form leaves whatever it likes in
  `r10`, and reading that as a thread id would boost a thread chosen by leftover
  register contents.

`SYS_GETTID` (186) came with it, because there was no way for a thread to learn
its own id: `getpid` answers for the process, and `sched_setattr` already spoke
thread ids userspace had no way to obtain.

`edos_lib::sync::PiMutex` is the consumer — owner tid in the word, `WAITERS` in
the top bit — and `programs/pitest` is the gate. Userspace cannot ask what CPU
time a thread has had, so it measures a controlled difference instead: the same
fixed work inside the section, against the same hogs, once with nobody waiting
and once with a priority-15 thread blocked on the lock.

```
pitest: 4 cpu(s), 16 hogs at priority 12
pitest: section 66602 us with nobody waiting, 15459 us with a priority-15 waiter (4.30x)
```

4.30x against the 4.3x the weight table asks for at four hogs per CPU, and a
2.00x gate. Watched red at **0.95x** with the owner argument passed as zero,
which is exactly a plain `futex_wait`.

Two things the shape needs. The hog count comes from `/proc/sched` rather than a
constant, and it is four per CPU rather than two: at two, a 4-CPU guest read
1.77x where one CPU read 3.86x, because the balancer kept finding the holder a
less crowded CPU. The fix is to leave nowhere less crowded. And the settle
window before the clock starts is outside the timed span — it is a fixed cost on
both sides, so leaving it in would not change which run is faster, but it would
drag the ratio towards 1 and understate the mechanism.

`pitest` is in `make guest-check`, which is 17 suites now.

## 7. Done: an idle CPU could halt with a runnable thread of its own (2026-08-19)

Found from the storage side, not from here: an NVMe raw sweep was 2-4x slower
than the same sweep on AHCI while posting a *better* median at every request
size, and the gap was one ~100.0 ms outlier per test. 100.0 ms is
`run_idle`'s fallback timer, so the stall was a CPU asleep on work it already
had.

`enqueue_ready` had three ways to tell a CPU it had work and all three could
miss together: `mark_running_thread_need_resched` is a no-op on an idle CPU,
`poke_idle_cpu` declines when `load() < 2` -- which is exactly one thread woken
onto an idle CPU -- and `has_work` is cleared on the way into `run_idle`, so an
enqueue racing that clear loses its flag. A thread landing between the decision
to idle and `publish_idle()` was invisible to `claim_idle_cpu` as well, and
waited for the timer.

Fixed by separating the two duties. `wake_if_idle` pokes the CPU the thread was
enqueued on, unconditionally; `poke_idle_cpu` keeps its guard, because
recruiting a *second* CPU to steal is only worth an IPI when there is surplus.
`run_idle` re-checks `queued()` -- not `has_work`, which can have been
clobbered -- after publishing itself idle, with a `SeqCst` fence on each side
so the two orders are exclusive.

Worth 1.9-2.2x on NVMe throughput and about 10% on AHCI at 1 MiB, and it
removes a ~100 ms tail from every IRQ-driven wake in the system. Full numbers
and the two refuted diagnoses in
`doc/bugs/2026-08-19-idle-cpu-halted-with-a-runnable-thread.md`.

**The lesson for this document: a mean hides a rare stall.** The wake path had
been measured here repeatedly and read healthy, because `switchbench` reports
averages over a hot loop where the CPU never idles. What caught it was a
benchmark that prints p50, p99 and max in the same row, on a workload that goes
idle between operations.

## What has been tried and did not work

- **A deadline-aware wakeup check.** Built and reverted 2026-08-15; the numbers
  and the reading that priced it are under "the four things EEVDF left open"
  below. 50 switches out of 2400 against a ±50 spread, because a wake is a small
  fraction of the switches on a machine whose slices expire every millisecond.

- **Bounding a carried lag by the thread's own request.** The natural reading of
  "a thread cannot be owed more than one turn", and it silently disables the
  slice: `vdeadline = (V - lag) + request` puts every thread carrying credit on
  `V` exactly, whatever it asked for. p95 wake 1007 us for a thread asking a
  quarter-slice, against 10 us with a fixed bound. Same section.

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

## Latency is measured on one CPU, and against throughput

`programs/latbench` is the instrument for the slice and for anything that
decides *when* a runnable thread runs. Two CPU hogs plus a thread that sleeps in
a loop; the report is how late its sleeps returned, at p50, p95 and max, beside
the hogs' throughput and the switch delta from `/proc/sched`. Both prices have
to be on the page: a shorter wait is only ever bought with more turns, and a
reading that shows one without the other can be made to say anything.

**Single CPU**, for the same reason `switchbench` wants one — with a spare CPU
the waking thread is served immediately and there is no latency to measure.

Three modes: the default prices a *per-thread* request (hogs at the kernel's
default, only the sleeper's slice changing, in both directions); `sweep` moves
every thread's slice together, which is the machine-wide setting; `clamp` checks
what the kernel grants for a request outside the range it serves.

**A reading must run for a fixed wall-clock window, not a fixed number of
rounds.** A late sleep stretches the round it is in, so counting rounds hands a
slow reading's hogs more real time and its throughput column climbs with the
very latency it is supposed to price against. That artefact showed throughput
rising 79% across the sweep; the honest answer is flat to within 3%.

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
`CURRENT QUEUED LOAD STEALS REBAL`) beside it, which is what placement believed.

`balancebench wake` prices the other half. The straggler test above *spawns* its
workers, and a spawn already goes to the least-loaded CPU through
`pick_sched_for`; a **wake** deliberately does not, because `complete_wake`
enqueues on the waker's CPU for cache locality. So it parks a worker per spare
CPU, lets the machine go fully idle, and times a burst of wakes from one thread.
Everything that spreads that burst is work-stealing, which makes the mode a
direct measure of how fast an idle CPU learns there is something to steal.
`median burst / solo` is its report on the same scale.

`balancebench sleep` prices the third case, and it is the one neither of the
others can reach: a **sleeper** is placed by nobody at all. A spawn asks
`pick_sched_for`, a park is placed by its waker and so follows the work, but the
sleepers heap is per CPU and hands the thread straight back to whichever CPU it
slept on. The mode wakes a burst the way `wake` does and then has each worker
sleep before it works, so what it asks is whether the concentration one round
created is still there the next. Its report is `(median burst − sleep) / solo`,
the same scale again, which is what makes it readable beside `wake fanout`: the
two agreeing means the sleep path costs what the park path costs.

`balancebench crowd` prices the fourth case, and it is the one the other three
structurally cannot reach: moving work between two CPUs that are **both busy**.
`wake` and `sleep` both end with a burst on one runqueue and the rest of the
machine halted, and what rescues them is a poke — an enqueue that leaves two or
more threads queued claims a CPU out of `IDLE_CPU_MASK` and IPIs it. That path
needs an idle CPU to claim. When every CPU is already running something there is
none, and periodic rebalancing on the timer tick is the only thing left. So the
mode keeps a spinner per CPU running for its whole length, which takes the idle
mask permanently empty, and then wakes a burst into it. Its floor is 2.00 rather
than 1.00, since a burst worker shares its CPU with a resident even when
placement is perfect. Read the `REBAL` column of `/proc/sched` beside it: it
counts only the threads periodic rebalancing moved, so a bad fanout with REBAL
at zero says the dial never fired rather than that it fired and did not help.

### Done: rebalancing every tick rather than every tenth (2026-08-18)

`REBALANCE_INTERVAL` was 10, commented "~50ms at 5ms timeslice" — and both
halves had stopped being true. The slice is a per-thread request defaulting to
1 ms, and the tick is not the slice at all: the timer is a one-shot armed for
whichever comes first of the running thread's deadline and the next sleeper's
expiry. What 10 actually bought was a correction rate of one thread per CPU per
~10 ms, against bursts that are over in less — an imbalance outliving the burst
that caused it is not corrected at all.

`balancebench crowd`, 8-CPU boot, median of three runs on a quiet host, where
2.00 is a perfectly spread burst and 9.00 is the whole of one behind a single
resident:

| interval | fanout |
|----------|--------|
| 10       | 3.99   |
| 4        | 3.16   |
| 1        | 2.36   |

The cost is a `SCHEDULERS` read and a walk of the registered CPUs on the tick
path, and it did not show: a solo CPU-bound lump on those same boots read
4.40 ms at an interval of 10 and 4.41 ms at 1. That walk is O(CPUs) under a read
lock though, and 8 is where this was measured, so the throttle stays in the code
as the first thing to reach for if a much larger machine ever finds the scan.

`REBALANCE_THRESHOLD` stays at 2, and is now measured rather than assumed:
dropping it to 1 at an interval of 1 moved the fanout not at all (2.36 either
way). It should not have, and the reason is worth keeping — the quantity
includes the running thread on both sides, so at a threshold of 1 a CPU at load
1 would steal from a CPU at load 2 and leave the pair exactly as unbalanced with
a migration paid for. Two is the smallest difference a single move can reduce.

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

Still open in the same area: `REBALANCE_THRESHOLD = 2` and
`REBALANCE_INTERVAL = 10` are picked numbers that nothing has measured against
the new quantity.

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

**The residual 2.0 it left** is closed two sections below, and the explanation
first written here — the last wake of a burst finding a drained queue — was
wrong.

### Done: a sleeper's expiry wakes a halted CPU too (2026-08-18)

The section above gave `enqueue_ready` a poke, and every enqueue path went
through it except one. `wake_sleepers` open-coded its own: pop the expired
entry, mark it `Ready`, push it on this CPU's runqueue, set `has_work`, ask the
running thread to reschedule. No poke, so a halted CPU learned about the sleeper
only when its own backoff poll came round, which is the tail this repository
already measured in *seconds*.

That path is the one where it matters most, because a sleeper is placed by
nobody. A spawn asks `pick_sched_for` for the least-loaded CPU. A park is
enqueued by its waker and so follows the work. But the sleepers heap is per CPU
and `transition_sleep` pushes onto whichever CPU the thread happened to be
running on, so a thread that sleeps in a loop keeps that CPU for as long as it
lives, however busy it becomes and however much of the machine is asleep. Work
stealing cannot rescue it either: stealing only reaches threads that are already
queued somewhere, and a sleeper is in no runqueue at all. On an idle desktop
`edos-wm` and `edos-taskbar`, the two busiest things running, both sit
`Sleeping` on CPU 0 with six CPUs empty.

`wake_sleepers` calls `enqueue_ready` now, which is the whole change: the
duplicated body goes, and the poke, the trace event and the assertions come with
it. `WakePriority::Normal` is what the inline version already asked for.

**What it is worth, measured** — `balancebench sleep`, 8-CPU boot, 7 workers,
median of 10 bursts, three runs a side, quiet host, same userspace binary either
side:

| | sleep fanout (median burst − sleep, over solo) |
|---|---|
| poll only (before) | 5.34, 3.69, 3.56 |
| poked (after) | 2.00, 1.99, 2.22 |

The spread on the before side is the mechanism showing through: what it costs
depends on whether the backoff poll happens to come round during the burst, so
the same kernel scores 3.56 and 5.34 on consecutive runs. After the change it is
2.0 every time — and 2.0 is exactly where `wake fanout` sits, so the sleep path
now costs what the park path costs and the residual has the same explanation as
the paragraph above.

### Done: the poke and the steal count the thread that is running (2026-08-18)

The two sections above each gave an enqueue path a poke, and both left the same
2.0 behind. The worker-side number in the report is what says where it went:
with `solo` at 4.4 ms the slowest worker's own loop read 8.3 ms, so that worker
was not waiting to be placed. It was *running* the whole time, sharing a CPU
with another worker for the length of the lump, while two CPUs sat halted.

Two rules kept the pair together, and both counted the same wrong thing:

- `poke_idle_cpu` returned early on `queued() < 2`, so a CPU running one thread
  with a second queued never poked anybody.
- `try_steal` skipped a victim whose `total_len() < 2`, so that same CPU was
  refused as a victim. A CPU that *was* poked found nothing eligible and went
  back to a halt of up to 100 ms — inside which the whole burst finished.

The quantity both wanted is `load`, the queue plus the thread running now, which
is the unit of work the scheduler already counts in (§ *load is runnable work,
not membership*). A CPU running one thread with another queued has a thread to
spare exactly as much as one with two queued does, and it is the commoner shape,
because the first thing a CPU does with a queue of two is run one of them.
`try_steal` still refuses to leave a victim with nothing to run — that rule is
what stops one thread ping-ponging between idle CPUs — but it counts what the
victim runs.

`tick_finish` pokes after re-enqueuing a preempted thread. A preemption is an
enqueue like any other, and two threads taking turns on one CPU are two runnable
threads that nothing else announces.

**What it is worth, measured** — 8-CPU boot, 7 workers, median of 10 bursts,
three runs a side, quiet host, same userspace binary throughout:

| | wake fanout | sleep fanout |
|---|---|---|
| before | 2.26, 1.95, 2.05 | 2.06, 2.13, 2.18 |
| poke and steal count `load` | 1.43, 1.45, 1.42 | 1.61, 1.60, 1.52 |
| plus the `tick_finish` poke | 1.19, 1.12, 1.19 | 1.19, 1.21, 1.20 |

`balancebench` imbalance stays at 1.01, so spawn-time placement is unchanged.

**Refuted in the same session: a poke from `spawn_thread`.** It was the third of
the three sites the paragraph above named, and it is the one that buys nothing.
`spawn_thread` already IPIs the CPU it chose and `pick_sched_for` chose the
least-loaded one, so an extra poke can only fire when every CPU has work — and
then there is no halted CPU to claim. Measured in and out: wake 1.21/1.21/1.22
with it against 1.19/1.12/1.19 without, sleep 1.20/1.25/1.22 against
1.19/1.21/1.20. It is not there.

**Cost.** `switchbench`, single-CPU boot, three runs a side against a stock
build on the same host and the same hour: pipe echo 436 against 434 ns, bad-fd
read 143 against 143, `getpid` 81 against 84 — all inside the run-to-run spread.
The switch path never reaches the poke, because a `pipe echo` that does not
block never enqueues. On an idle 8-CPU desktop over 41 s the switch count is
unchanged (6696 against 6721) and steals go from 60 to 184, about three more per
second, which is the change doing its job rather than an overhead.

### Done: the priority buckets are EEVDF now (2026-08-15)

The runqueue was sixteen strict-priority lists with an anti-starvation escape,
and every pick handed out the same 5 ms. Both halves were wrong, and the second
was worse than `doc/AUDIT.md` §4 claimed.

**What the buckets actually did.** Strict order alone starves, so `pop_next`
serviced a lower level every `STARVE_STREAK_LIMIT` picks — which fixed a real
deadlock in 2026-08-08 and left a hole nobody had looked for. The escape went to
the highest non-empty level **below the top**, so with three levels occupied on
one runqueue the bottom was never reached at all. That is not a corner case: the
default is 7, block I/O runs at 8, and an interrupt-priority wake landed at 9 or
10. Measured on a CPU carrying an `IO_PRIORITY` kthread, seven priority levels
bought **58x** of the CPU — not the fixed 2:1 the escape was thought to
guarantee, and not a share at all.

**What replaced it.** Each thread has a weight (1.25x per priority level), a
virtual clock that advances at `delta * 1024 / weight`, and a virtual deadline
of `vruntime + slice/weight`. `V` is the queue's weighted average; a thread
behind it is *eligible*; the pick is the eligible thread with the earliest
deadline. Starvation stops being a rule to maintain and becomes structural: a
passed-over thread falls behind `V` and its deadline is already in the past.

Three deliberate departures from Linux, all in `thread/runqueue.rs`:

- **A linear scan, not an RB-tree.** Linux needs the tree because a runqueue can
  hold thousands; one here holds single digits, and the pick it replaced was
  already an O(16) walk of the buckets.
- **`V` is recomputed each pick, not maintained.** Same reason `Scheduler::load`
  is derived: it stays a fact about the queue rather than a running sum that
  every enqueue, dequeue and steal must remember to adjust.
- **The interrupt-wake boost is a shorter request, not a heavier weight.** The
  buckets said "run this sooner" by lending two priority levels, which also
  handed the thread a larger *share* for as long as it stayed runnable. A
  smaller request is an earlier deadline and expires on its own.

The slice became a per-thread request. `BASE_SLICE` is 1 ms, derived rather than
picked: arming the APIC one-shot costs ~1 us because a hypervisor traps it, so
1 ms keeps arming under 0.15%, and at ~13 ms per compositor frame it lets about
thirteen runnable threads each take a turn per frame.

**Measured.** Share now tracks weight — seven levels bought **4.84x** against
the 4.77x the weight table asks for, a 1.5% error. The switch path got *faster*,
because the old pick walked sixteen levels from the top and `pop_lower_than`
walked back down every third one:

| single-CPU boot | buckets | EEVDF |
|---|---|---|
| `sched_yield`, handover to a sibling thread | 365 ns | **290** |
| `sched_yield`, handover to another process | 420 ns | **354** |
| `sched_yield`, nothing else Ready | 255 ns | **235** |
| pipe round trip, two processes | 2210 ns | 2174 |
| `getpid` (control) | 81 ns | 83 |
| pipe echo (control) | 422 ns | 427 |

The two controls take no scheduler pick and did not move, which is what makes
the rest attributable. Placement and throughput are unchanged: `balancebench`
imbalance 1.00, wake fanout 1.99/2.01/2.05 against 1.97-2.03 before, and a solo
lump of 152.3 ms against the 152 ms on record.

Both gates were watched fail against the bucket scheduler, which is the whole
reason the 58x above is a measurement rather than a theory: `weighted-share`
(seven levels must buy more than 3x and less than 9x) and
`starvation-three-levels` (three levels pinned to one CPU, the bottom must run).
`thread/sched_test.rs`, 52 → 54 tests.

**Left open at the time.** Lag across a sleep, a deadline-aware wakeup check,
`BASE_SLICE` against a workload rather than a derivation, and a syscall for the
per-thread slice. All four are answered below.

### Done: the four things EEVDF left open (2026-08-15)

The instrument came first, because three of the four are latency questions and
nothing in the tree could measure latency. `programs/latbench` runs two CPU hogs
against a thread that sleeps in a loop and reports how late its sleeps returned,
beside the hogs' throughput and the machine's switch count — the two prices any
answer here is paid in. **Single-CPU boot**, for the reason `switchbench` wants
one: a spare CPU serves the waking thread at once and every reading collapses.
`/proc/sched` gained a `SWITCHES` column to feed it.

**1. Lag is carried across a sleep now, and it was closing a real hole.** A
thread's lag — `V - vruntime`, positive for under-served — is recorded when it
leaves the runnable set and handed back by `RunQueue::place`. Placing a waking
thread level at `V` forgave whatever it owed, and the shape that abuses it is
ordinary: burn a slice, sleep for a tenth of one, repeat. Each cycle hands back
a full slice of overrun for free. Gated by `burst-share`, which measures the CPU
each of a pair actually received. The direction is the opposite of what this
entry first recorded, and the numbers here are superseded — see "`burst-share`
was gating the wrong direction" above: with the lag carried the sleeper reads
**1.00x to 1.01x**, and with `place` reverted **0.73x to 0.91x**, because what
it leaves owed is its own sleep rather than an overrun.

**It costs ~22 ns per park/wake pair** and nothing anywhere else. `switchbench`
on a single-CPU boot, against trunk at `713ed5a`: a cross-process pipe round
trip 2180 → 2224 ns (median of three, spread 2215–2237), with `yield idle` at
238 against 239, `getpid` 81 against 82 and the pipe echo 426 against 424 — the
controls unmoved, which is what makes the round-trip figure attributable. The
cost is one extra runqueue acquisition and one `avg_vruntime` scan on the path
where a thread stops being runnable; `pick_next` recomputes the same `V` a
moment later, so merging the two would take most of it back, at the price of
threading the outgoing thread into the pick. Not worth it against 2% of a
blocking round trip in the most delicate function in the tree.

Two things that were *not* free and had to be found by measuring: reaching the
outgoing thread through `current_thread()` costs an `Arc` refcount pair on every
switch, including the far more common one where there is no lag to record, and
putting the new `switches` counter in the middle of `Scheduler`'s hot fields
moved them across a cache line. Together they read as **+43 ns on `yield idle`**
— a case that records no lag and counts one switch — until the counter went to
the end of the struct and the lookup went through the per-CPU slot.

The clamp is one `BASE_SLICE` of virtual service either way, and **it must not
be the thread's own request**, however natural that reads. A placement sets
`vdeadline = (V - lag) + request`, so bounding the lag at `request` cancels the
request out of the deadline: every thread carrying credit lands on `V` exactly,
whatever it asked for. That cost a thread asking for a quarter-slice its entire
latency advantage — p95 wake **1007 us**, against **10 us** once the bound
stopped moving with the request. The fixed bound also removes the need for a
decay, which is the other half of what made this look expensive: a sleeper of
any length returns with at most one slice of credit.

**2. The wakeup check was built, measured, and taken back out.** `enqueue_ready`
still marks the running thread `NEED_RESCHED` on every enqueue regardless of
whether the waking thread's deadline is earlier. The Linux-shaped fix
(`check_preempt_wakeup_fair`: charge the running thread, compare deadlines,
decline when it still wins) was implemented and priced against the reading built
to expose it — a sleeper asking for four times the hogs' slice, whose deadline
is far enough behind theirs that every preemption it requests is a save, a pick
that chooses the hog again, and a restore:

| sleeper asking 4 ms | switches over 1.8 s |
|---|---|
| preempt on every enqueue | 2396 |
| preempt only on an earlier deadline | 2346 |

50 switches out of 2400, on a column whose run-to-run spread is ±50 on the same
build. Latency and throughput did not move on any reading. **A wake is a small
fraction of the switches on this system** — two hogs alternating on a 1 ms slice
produce a switch every millisecond whatever wakes, so there is little for the
check to save and it is paid for on every wake. Reverted; re-run
`latbench`'s "sleeper asking 4 ms" line before building it again, and expect a
different answer only from a workload where wakes outnumber slice expiries.

**3. `BASE_SLICE` stands at 1 ms, now measured rather than only derived.**
`latbench sweep`, every thread at the same slice, single CPU:

| slice | p50 wake | p95 wake | hog throughput | switches |
|---|---|---|---|---|
| 250 us | 5 us | 8 us | 39.4 chunks/ms | 7728 |
| 500 us | 5 us | 7 us | 40.1 | 4184 |
| **1 ms** | **5 us** | **1007 us** | **40.5** | **2382** |
| 2 ms | 15 us | 2019 us | 40.5 | 1492 |
| 4 ms | 6 us | 6148 us | 40.7 | 1044 |
| 10 ms | 11016 us | 19529 us | 40.5 | 758 |

**Throughput is flat to within 3% across a 40x range of slice**, which refutes
the reason a longer one would be chosen: the switching and arming a 250 us slice
costs are worth 2.7% of hog throughput, not the double-digit figure the
derivation guards against. So there is no throughput pressure to raise the
slice, and no latency reason to lower it either — the wake tail is set by the
*difference* between the waker's request and the holder's, not by the absolute
value, which is why 250 us and 500 us read 8 us here while 1 ms reads 1007. The
way to get latency is to ask for less than the holder, which is item 4. What the
sweep does settle is the ceiling: at 10 ms an ordinary sleeper misses its wake by
more than a compositor frame, which is where `MAX_SLICE` sits.

An earlier form of this table showed throughput climbing 79% with the slice and
it was entirely an artefact — a reading that counted a fixed number of sleep
rounds runs for longer when its sleeps return late, so its hogs got more real
time to work in. `latbench` measures over a wall-clock window for that reason.

**4. `sched_setattr`/`sched_getattr` (314/315) expose the slice and the
priority.** Nothing set `slice_ns` before, so the knob EEVDF exists to provide
was unreachable from userspace. The measurement that justifies it, hogs at the
kernel's default and only the sleeper's request changing:

| sleeper's request | p50 wake | p95 wake | switches | hog throughput |
|---|---|---|---|---|
| 1 ms (the default) | 5 us | 1008 us | 2381 | 40.3 chunks/ms |
| 250 us | 5 us | **11 us** | 2400 | 40.4 |

A hundredfold on the tail for 19 switches and no throughput taken from anyone
else — the shortened p95 reads 7 to 11 us across runs and the switch delta 19 to
28, both against a 1008 us baseline that does not move — which is the whole claim EEVDF makes for a slice being a request rather
than a quantum. The request is clamped to `MIN_SLICE ..= MAX_SLICE` rather than
rejected, and `sched_getattr` reports what was granted; `latbench clamp` checks
that. There is no privilege check because the system has no user model, and
EEVDF is what makes that tolerable — the top of the priority table buys 6x of a
share, not a lockout.

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
140–141 ns at `652b35c` and 169 at `7be0b37`, so that one commit costs **+29 ns
on every failing syscall**. The mechanism is the line it added to
`syscall_handler` — `current_thread_info().lock().errno` — which was a second
registry lookup on the way out. The per-CPU cache above takes it back without
giving up the feature, and the same bisect priced the rest of the window:

| | bad fd | pipe echo |
|---|---|---|
| 2026-08-11 (recorded) | 128 | 387 |
| `d965f88`, end of 08-13 | 144 | 452 |
| `652b35c`, before negative errno | 140 | 450 |
| `7be0b37`, negative errno | 169 | 450 |
| now, with the info cache | 145 | 425 |
| one fd-table walk per call | 137 | 410 |

So the error path is recovered, and of the ~38 ns the pipe echo had lost on the
*successful* path, **14 are back and ~23 are still unaccounted for.**

The 14 were `3c24e7c`'s. `O_NONBLOCK` outliving the open meant a read and a
write each asked the fd table two questions — `get_fd` then `is_nonblock` —
under one lock but as two searches of the same `BTreeMap`. `get_fd_nonblock`
answers both in one walk, and the size of the effect identifies it: the bad-fd
read, which is one call and one lookup, fell 145 → 137, and the pipe echo, which
is two calls and two lookups, fell 425 → 410. Two independent readings of a
~7 ns map walk. `getpid` did not move, which is what makes both attributable.

What is left is ~23 ns and the same window, so `e5f22f3` (named pipes, the
bounded PTY) is now the standing suspect by elimination rather than the weaker
of two. It added a `PipeReadWrite` variant that every match on `FileDescriptor`
in the read and write paths carries. **That is a guess, not a measurement** —
settling it needs the whole-commit bisect this thread has always wanted, and the
harness is in `doc/WORKING-NOTES.md`.

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
