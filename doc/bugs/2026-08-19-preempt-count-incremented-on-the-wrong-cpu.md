# The preemption count was raised on one CPU and lowered on another

## Status

Fixed. `kernel/src/thread/preempt.rs` — `preempt_disable`, `PreemptGuard::drop`
and the count's reader are single GS-relative instructions rather than a GS-base
read followed by an access through the register. `kernel/src/memory/tlb.rs` —
`tlb_shootdown` suppresses preemption before it reads which CPU it is on, which
was the same defect one layer up.

Gate: `scripts/guest-check` under QEMU 8.2, which had never passed and is now
17/17. The command is in `doc/ci.md`.

## Symptoms

CI's `guest suites` job failed identically on every run it ever had: `mmaptest`
exited 1, the guest went quiet, and the remaining fourteen suites each spent
their full budget on a timeout. The serial log ends in

```
thread_park_while with preemption disabled (spin lock held across a blocking operation)
```

on the **reaper kthread**, which takes no spin lock and holds no guard when it
parks. `make guest-check` on a workstation running QEMU 10.0 was 17/17 green,
and stayed green pinned to two host CPUs, so it was never the runner's core
count. It is a race, and QEMU 8.2 lands interrupts in the window often enough to
hit it inside one boot.

Read the panic as "some *other* thread left this CPU's count non-zero". The
thread named in the message is the victim: the first one to park on a CPU whose
count was already wrong.

## Root cause

The count is per-CPU (`PerCpuData::preempt_count`), and `preempt_disable`
reached it in two steps:

```rust
get_percpu_data()               // rdgsbase -> register
    .preempt_count
    .fetch_add(1, Ordering::Acquire);   // lock xadd through that register
```

The caller is still fully preemptible between those two instructions, because
the count that would stop it has not been raised yet. A timer tick landing in
that window deschedules the thread; another CPU picks it up; the increment then
lands on the CPU it *left*. `PreemptGuard::drop` runs on the CPU it arrived on
and decrements that one, and `fetch_sub` on zero wraps to `u32::MAX`.

Two CPUs are wrong from that moment and neither ever recovers. The one that was
left keeps a count nobody will lower, so `maybe_preempt` declines forever and
nothing on it is ever preempted again. The one that wrapped is non-preemptible
for the same reason, and the next thread to park there trips the assert.

The invariant the code meant to have is that **the pair of operations happens on
one CPU**, and the mechanism that was supposed to guarantee it is the count
itself: once raised, the thread cannot be moved. That argument is sound, and it
only fails during the raise. An instruction cannot be split by an interrupt, so
performing the increment as one GS-relative `add` closes the window completely —
before it, migration is possible but nothing has been written; after it, a write
has happened but migration is not.

`tlb_shootdown` had the same shape without the count: it read
`current_cpu_index()` and built the target mask *before* suppressing preemption.
A caller moved in that window exempts the CPU it left, which is the one that
keeps the stale translation, and then waits for an acknowledgement from the CPU
it is running on, which was never sent an IPI.

## Reasoning rules going forward

- **A per-CPU location must be read and written under one protection, and the
  protection must already be in force when the CPU's identity is read.** Every
  other per-CPU accessor in the tree already did this — the allocator's
  `heap_cache` is reached inside `without_interrupts` — which is what made
  `preempt_disable` the odd one out.
- **The thing that makes a section non-migratable cannot itself rely on being
  non-migratable.** Suppressing preemption, pinning to a CPU, claiming a per-CPU
  slot: the acquire step of each runs unprotected by construction, so it has to
  be a single instruction or run with interrupts off.
- **A counter that can wrap below zero turns a transient fault into a permanent
  one, on a CPU with no owner to blame.** `PreemptGuard::drop` now asserts the
  count is non-zero before lowering it, so an unbalanced release names itself
  instead of wedging a CPU for some later thread to trip over.
- A panic that names a thread holding nothing is evidence about *state the
  thread inherited*, not about that thread. Look for who wrote the state, not
  for what the victim was doing.

## If this reappears

1. `thread_park_while with preemption disabled` naming a kthread that takes no
   locks is this class. The same message naming a thread that genuinely holds a
   `PreemptSpinlock` guard is the ordinary bug — a lock held across a park — and
   the fix is in that caller.
2. `preemption count underflow` is the assert added with this fix, and it names
   the CPU whose count went wrong at the moment it went wrong rather than
   letting a later thread find it.
3. Confirm the instruction sequence is still one instruction:
   `objdump -d kernel/target/x86_64-unknown-none/debug/edos-kernel | grep '%gs:'`
   should show `addl $0x1,%gs:<off>` and `subl $0x1,%gs:<off>`, not a `rdgsbase`
   followed by a `lock xadd`.
4. Reproduce on QEMU 8.2 rather than a current one; the container command is in
   `doc/ci.md`. A workstation QEMU may take many boots to show it.
