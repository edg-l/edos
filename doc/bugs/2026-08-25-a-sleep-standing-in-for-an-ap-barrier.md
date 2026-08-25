# A 100 ms sleep standing in for an AP barrier

## Status

Fixed. `smp::init` waits on `AP_READY_COUNT` rather than on the clock, and the
second sleep in `main` is gone. Boot to the taskbar's first `panel|` line, same
host and same day, three runs each:

| vCPUs | before | after |
|---|---|---|
| 1 | 0.715 – 0.722 s | 0.515 – 0.520 s |
| 4 | 0.457 – 0.471 s | 0.281 – 0.294 s |
| 16 | 0.535 – 0.538 s | 0.429 – 0.482 s |

## Symptoms

None. Nothing failed, and that is the point: this was found by profiling, not
by a bug report, and the race it left open never visibly fired.

`scripts/perf-kvm` over a cold boot put `<edos_kernel::timer::Instant>::elapsed`
at the top of the profile with 13.16% of guest cycles. Two spin loops accounted
for it, each waiting a flat 100 ms:

- `smp::init`, after asking Limine to bootstrap every AP.
- `main`, immediately after `smp::init` returned.

The serial log agreed on its own, two gaps of 102 ms with nothing logged in
between: `mark_kernel_mappings_global` → `SYSCALL/SYSRET enabled` brackets the
first, and `Evict kthread started` → `Enabled FPU and SSE` the second.

## Root cause

`cpu.bootstrap(ap_start, 0)` starts an AP **asynchronously**, and nothing joined
on it. The sleep was the join. It was not written as one — it carried no comment
and grew from 50 ms to 100 ms in a commit titled "fix wake" — but it was the
only thing standing between the BSP and a CPU that had claimed an index in
`ONLINE_CPU_MASK` without yet having a scheduler, a synced TSC, an FPU or its
syscall MSRs.

A timed sleep cannot be a barrier, because the quantity it guesses at scales and
the constant does not. Measuring the thing it was guessing shows exactly where
it stopped covering: on a 16-vCPU boot the APs report in from 78 ms to 178 ms,
so at the moment the old code stopped waiting, **2 of 15 APs were ready**. The
BSP then went on to init the drivers, spawn kthreads and let the scheduler place
work on thirteen CPUs that were still initialising themselves.

The second sleep was not even that. `749ad826` added it directly before a
`test_new()` scratch harness that spawned three mailbox threads; that harness is
long gone and the call was left commented out on the following line. It was
scaffolding that outlived what it was scaffolding.

The fix is the barrier the first sleep was impersonating. `ap_start` raises
`AP_READY_COUNT` as its **last** act, after `thread::scheduler::init` and
`enable_percpu_cache`, and `init` spins until every AP Limine reported has
arrived. `AP_BRINGUP_TIMEOUT` is a ceiling on a broken machine rather than a
delay on a working one: it is never reached when the APs are healthy, and
reaching it logs how many arrived and carries on.

## Reasoning rules going forward

- **A sleep is not a barrier.** If code waits a fixed time for something else to
  happen, the thing it is waiting for has a name and can be signalled. Wait on
  the signal and keep the duration only as a ceiling.
- **`CPU_COUNT` and `ONLINE_CPU_MASK` mean "has claimed an index", not "can run
  a thread".** An AP raises both as its first act so that `current_cpu_index`
  can answer for it during the rest of its own initialisation. `AP_READY_COUNT`
  is the one that means ready.
- **A constant that was right once is not right at another size.** 100 ms
  covered four vCPUs and did not cover sixteen, silently, for a year.

## If this reappears

A CPU behaving as though it has no scheduler, a TSC that disagrees, or an FPU
fault on an AP early in boot: read the serial log for `[smp] N of M APs online
after timeout`. That line means the barrier gave up, and the system is running
in exactly the state the old code always ran in. Raise `AP_BRINGUP_TIMEOUT` only
after checking the spread of the `[smp] AP online` timestamps — if the last one
lands beyond the ceiling, bringup got slower and the ceiling is not the defect.

To see the cost of a boot-path change at all, profile the boot with
`scripts/perf-kvm`, started before the guest. The in-guest `profile` cannot:
it needs userspace, which does not exist yet. See `doc/profiling.md`.
