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

## Where things stand (2026-08-11)

| | ns |
|---|---|
| `sched_yield`, nothing else Ready | 433 |
| `sched_yield`, handover to a sibling thread | 490 |
| `sched_yield`, handover to another process | 751 |
| **park/wake, one direction** | **2276** |
| pipe round trip between two processes | 4552 |
| the switch itself, `/proc/sched_prof` | 220 |

Inside the 220 ns switch: `page` 66-77, `fxrstor` + `fxsave` 91, `CpuContext`
copies 36, publish 19, transition 27, `wake_sleepers` 18, pick 12, timer 10.

**The switch is no longer the expensive part.** A wake costs about four times
the switch it performs, and it is what every real workload here pays — a shell
pipeline, the compositor talking to a client, the terminal. Items 1 and 2 are
both about that gap; item 1 is the smaller and safer of the two.

## 1. A voluntary switch does not need a register frame or an `iretq`

`sched_yield` is 433 ns and the switch inside it is 220, so ~210 ns is the
boundary: `save_transition_switch` builds a synthetic 160-byte `CpuContext` on
the stack, `do_save_current_thread` copies it into the thread under a spin
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

Worth: the 36 ns of `CpuContext` copies plus the `iretq` and the frame build,
so a large fraction of the 210 ns boundary.

Reference: <https://kernel-internals.org/sched/context-switch/>

## 2. A blocking IPC should hand off directly to the receiver

A pipe round trip is 4552 ns for two switches that cost 490 ns each as plain
handovers. The rest is the wake path: the writer enqueues the reader on a
runqueue, possibly sends a reschedule IPI, then parks; the scheduler then picks
the thread that was just enqueued.

L4 and seL4 answer this with a **direct process switch** — the sender yields
straight to the receiver, on the sender's own timeslice, with no runqueue
operation and no scheduler invocation, guarded by a fastpath that requires the
receiver to be runnable here and nothing of higher priority to be waiting.
seL4 reaches 0.2-0.5 us round trips that way, against our 4.5.

The shape for us: when a thread wakes exactly one target, that target is
allowed on this CPU, and the waker is about to block anyway, switch to it
directly rather than enqueue-then-park-then-pick. Everything else falls back
to the path that exists. The fastpath conditions are the design work; seL4's
list is a good starting point and the reasons for each are documented.

References: <https://docs.sel4.systems/Tutorials/ipc.html>,
<https://microkerneldude.org/2019/03/07/how-to-and-how-not-to-use-sel4-ipc/>

## 3. Spin briefly before parking

Independent of 1 and 2, and much smaller: `BlockingMutex` and the wait queues
park immediately on contention. Spinning first for about the cost of a switch
turns a short wait into no switch at all. LWN reports a futex microbenchmark
going from 35M to 54M operations with adaptive spinning.

The bound should be derived from the measured switch cost rather than picked,
and the spin should give up if the holder is not currently running — which is
the check a pure spinlock cannot make and the reason this is called *adaptive*.

Reference: <https://lwn.net/Articles/386536/>

## 4. `switch_to_page` takes a lock to decide it has nothing to do

66-77 ns, on every switch including the common one where the address space
does not change. It takes `user.read()` — an `RwLock` — and reads `CR3` before
comparing. Mirroring the thread's `CR3` in an `AtomicU64` removes the lock;
only three sites set it (thread creation in `thread.rs`, `execve`, `fork`).

Getting this wrong means a thread runs on the wrong address space, so the
mirror has to be provably updated at all three.

## 5. `MIN_TIMER_INTERVAL` is still a picked number

The 10 us floor in `apic/mod.rs` was chosen to sit above interrupt cost and
below a 1 ms sleep, not measured. It matters much less now that the one-shot
is re-armed rarely. TSC-deadline mode would remove the need for a floor at all
(an absolute deadline rather than a counted-down interval) and is the other
reason to consider it; it is **not** a way to make arming cheaper, since it is
still a trapping MSR write.

## What has been tried and did not work

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

## Recently closed

The 2026-08-11 round, in `doc/WORKING-NOTES.md`: the APIC one-shot is re-armed
only when what is armed would fire too late (that write alone was 1024 ns of a
1270 ns switch), `FS.base` goes through `rdfsbase`/`wrfsbase`, and kernel
mappings are `GLOBAL` with `CR4.PGE` on. `sched_yield` 1917 → 433 ns.

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
