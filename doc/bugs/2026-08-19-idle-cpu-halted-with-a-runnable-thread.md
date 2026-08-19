# A CPU halted with a runnable thread in its own runqueue (2026-08-19)

## Symptom

`fsbench raw /dev/nvme0n1` was two to four times slower than the same sweep on
`/dev/sda`, on every request size, in the same boot. The medians said the
opposite: NVMe read 4 KiB at a 42 us median against AHCI's 90 us, and 64 KiB at
91 us against 138 us. The command path was plainly faster and the throughput
was plainly worse.

The distribution explained it. Every NVMe test carried exactly one enormous
outlier against a tiny p99:

| request | p50 | p99 | max |
|---|---|---|---|
| 512 B | 1 us | 49 us | 100.0 ms |
| 4 KiB | 42 us | 57 us | 100.0 ms |
| 64 KiB | 91 us | 132 us | 100.1 ms |
| 1 MiB | 1.0 ms | 1.5 ms | 101.3 ms |

With a 700 ms budget per test, one 100 ms stall is most of the measurement.

## Reading the number

100.0 ms is not a plausible device time and it did not vary. It is the
constant in `Scheduler::run_idle`: a CPU with no earlier deadline arms its
timer for `now + 100ms` before halting. So the stall was not I/O at all. It was
a CPU asleep with work already waiting for it, and the fallback timer was the
only thing that ever noticed.

That also explained why AHCI was not immune -- one sweep showed a 142.9 ms max
-- and why the NVMe watchdog only occasionally covered for it. Nothing here is
storage-specific.

## Root cause

`Scheduler::enqueue_ready` publishes a newly runnable thread and then tries to
make some CPU look at it:

```rust
sc.has_work.store(true, Ordering::Release);
sc.mark_running_thread_need_resched();
sc.poke_idle_cpu();
```

All three can miss at once.

- `mark_running_thread_need_resched` is a no-op on a CPU that is idle: there is
  no running thread to mark.
- `poke_idle_cpu` opens with `if self.load() < 2 { return; }`. It is a
  *work-stealing* poke -- "I have surplus, recruit someone" -- and one thread
  woken onto an idle CPU never has surplus. The case that most needs an IPI is
  precisely the case it declines.
- `has_work` is cleared unconditionally on the way into `run_idle`, so an
  enqueue that raced that clear has already lost its flag. The idle loop then
  tests the flag it just zeroed.

The window is between the scheduler deciding to idle and `publish_idle()`
setting the CPU's bit in `IDLE_CPU_MASK`. An enqueue landing there is invisible
to `claim_idle_cpu` (the bit is not set yet) and its `has_work` is discarded.
The CPU halts with the thread sitting in its own runqueue and sleeps 100 ms.

`Scheduler::load`'s own doc comment had already drawn the distinction that
makes this a bug: a stale count "is a slightly worse placement, not a thread
nobody runs". The wake path was using the balancing predicate for the wake.

## Fix

Two halves, because there are two duties and they need different conditions.

`wake_if_idle` pokes the CPU a thread was *just enqueued on*, with no load
condition, and is called from `enqueue_ready` beside the existing
`poke_idle_cpu` -- which keeps its guard, since recruiting a second CPU to
steal really is only worth an IPI when there is surplus.

`run_idle` re-checks after publishing itself idle and before halting, against
`queued()` rather than `has_work`: the flag can have been clobbered, the
runqueue count cannot.

A `SeqCst` fence on each side makes the two orders exclusive. Either the waker
sees the idle bit and sends the IPI, or the idling CPU sees the enqueue and
does not halt. Without the fences both sides can read stale, which is the same
100 ms stall arrived at by a narrower path.

## Result

`fsbench raw`, best of five sweeps per device in one boot:

| request | NVMe before | NVMe after | AHCI before | AHCI after |
|---|---|---|---|---|
| 512 B | 32.4 MiB/s | 77.7 | 38.9 | 37.9 |
| 4 KiB | 24.3 | 92.4 | 42.0 | 41.2 |
| 64 KiB | 376 | 674 | 450 | 455 |
| 1 MiB | 450 | 996 | 892 | 983 |

Every 100 ms maximum is gone; the worst NVMe outlier is now 1.2 ms. NVMe ends
up faster than AHCI at every request size, which is what the medians said from
the start. AHCI gains too, most visibly at 1 MiB -- the expected shape for a
defect in code both drivers share.

## What this cost, and the lesson

The stall was found while asking a product question ("is NVMe faster than
AHCI?"), not while looking for a scheduler bug, and it had been in every boot
of every driver the whole time. Two things hid it:

**A throughput average buries a rare stall.** Only the max column made it
visible, and only because `fsbench` reports p50, p99 and max side by side. A
suite reporting MiB/s alone would have said "NVMe is slow" forever.

**The first two diagnoses were wrong, and cheap to check.** `complete_wake`
enqueueing without a poke -- refuted, `enqueue_ready` ends in `poke_idle_cpu`.
An affinity path sending threads to remote CPUs -- refuted,
`set_affinity_mask` has no caller outside `sched-test`. Reading the callee
each time cost minutes; believing either would have cost a rewrite of the
wrong thing.
