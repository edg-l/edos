# An idle CPU kept using a thread's kernel stack after publishing the thread

## Status

Fixed. Four defects surfaced together under one soak; each has its own commit:

- `f07133a4`: `IrqSpinlock::lock` waits with interrupts enabled.
- `f8e29a4d`: the root cause. `thread_exit` and the timer tick leave a
  thread's kernel stack before the thread is published.
- `3999a72a`: the TLB shootdown never reports a flush that did not happen.
- `09a886bd`: `pick_sched` samples `thread_count` once.

## Symptoms

Reproducible in about a minute: loop `threadtest`, `threadtest hammer` and
`threadtest nojoin` through `scripts/edos-vm` on a 4-core boot. The log turns
into nothing but

```
<cpu-2:bin/edos-wm:u:21> tlb_shootdown: timeout waiting for CPUs (mask=0x1), forcing clear
```

(314 times in one run), input stops while the taskbar clock keeps redrawing,
and CPU 0 ends in `double_fault_handler` with CPUs 1 to 3 idle. Another run
ended with three CPUs in `IrqSpinlock::lock` on the serial port and the fourth
in `tlb_shootdown` waiting for them.

Other shapes of the same corruption: interrupt frames whose
`instruction_pointer` holds an RFLAGS-looking value (`0x286`) and whose
`code_segment` index is past the end of the GDT, and a garbled log prefix
(`<cpu-633166472:kernel>`, uptime near `u64::MAX`).

With `--features trace` on 10 cores, the first iteration named it:

```
cpu 0:  [36] Save   cpu=0 tid=46 rip=0x412cd9
        [37] Switch cpu=0 46->50
cpu 9:  [13] Steal  0->9 tid=46
        [14] Switch cpu=9 0->46 rip=0x412cd9     <- from_tid 0: CPU 9 was idle
```

CPU 9 then panicked with `cw: Low context address 0x1`.

## Root cause

Two paths kept using a kernel stack after handing its thread to another CPU.

`run_idle` held `context`, a pointer to the interrupt frame, in a local across
`enable()` and `enable_and_hlt()`. On the timer-preemption path, that local and
the frame both lived on the outgoing thread's kernel stack, because the timer
handler did not pivot. `maybe_preempt` had already saved and enqueued the
thread, so another CPU could steal it and resume it on that same stack while
this CPU was still idling on it. Two CPUs then wrote one stack.

`thread_exit` had the same shape: `reaper_enqueue(t)`, then `switch_away()` on
the dying thread's stack, which `Thread::free` unmaps. `threadtest` exits about
forty threads a run, which is why it reproduced there.

The fix pivots to the per-CPU scheduler stack before publishing. `thread_exit`
only marks the thread `Dying`; `switch_away` pivots and `reap_and_schedule`
posts to the reaper from the scheduler stack. The tick is split into
`tick_prepare` (thread stack: save the outgoing context) and `tick_finish`
(scheduler stack: enqueue and pick), with the naked handler copying the
160-byte `CpuContext` between them. A const assert in `thread/context.rs` pins
the 160, since three trampolines hard-code it.

The other three defects:

- `IrqSpinlock::lock` disabled interrupts and then spun, so a waiter answered
  no IPIs for the whole wait. Serial-lock saturation (one VM exit per UART
  byte, a log line per thread exit) stretched that past the shootdown timeout.
  It now disables, tries, and re-enables around a read-only spin
  (`kernel/src/thread/irqlock.rs`). This alone took the run from 916 shootdown
  timeouts to 0.
- The shootdown timeout force-cleared `pending_mask` and returned, telling the
  caller no CPU held the old translation when one did; the caller then freed
  the page. `kernel/src/memory/tlb.rs` re-sends to outstanding CPUs and panics
  after `ACK_ATTEMPTS`, stamps each round with a `generation` so a late handler
  cannot credit the current round, and runs the round with preemption
  suppressed so the initiator cannot be descheduled holding `active`.
- `pick_sched` found the minimum `thread_count` in one pass and matched it in a
  second; spawns in between made the second pass match nothing and reach
  `unreachable!()`.

## Reasoning rules going forward

- Leave a thread's stack before publishing the thread. Once it is enqueued or
  posted to the reaper, another CPU may run on or free that stack.
- Interrupts need to be off only while a lock is held, not while waiting for it.
- A timeout that returns success is corruption with the evidence discarded.
  Stop loudly instead.
- Corrupted output names where corruption landed, not its cause: the garbled
  log prefix came from `_serial_print` formatting on a stack the exit path was
  corrupting.
- Vary the soak workload. Mixing `mmaptest` into the loop doubled the spawn
  rate and found `pick_sched`; repeating one workload never would.

## If this reappears

1. Shootdown timeouts or a shootdown panic naming a mask, double faults, or
   interrupt frames with RFLAGS-looking RIPs all point at a stack used by two
   CPUs.
2. Rebuild with `--features trace` and look for a `Steal` of a tid whose
   `Save` CPU then halted or kept running on the same frame.
3. Audit any new path that enqueues, steals or reaps a thread for work done on
   that thread's stack after the publish.
