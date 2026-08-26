# The reset dropped a command instead of failing it (2026-08-26)

## Status

**Fixed** in `NvmeQueue::reset_state`. This is shape A of
`2026-08-26-the-hostile-nvme-boot-is-two-bugs-and-neither-is-the-log.md`, and
it is a different defect from the heap corruption in
`2026-08-26-the-device-was-still-writing-into-the-buffer.md`.

A residual failure remains under `nvme_timeout_ms=0` with a *different*
signature, described at the end. It is not this.

## The defect

`reset_state` emptied the command slots in two passes:

```rust
for op in self.outstanding_ops() { retire_op(self, op, Err(Io), Abandoned) }
for slot in &self.cmd_slots { **slot.lock() = None }
```

A command installed between them is destroyed by the second pass. Nothing
retires it: no `inflight_dec`, no command id freed, no `DmaBuffer` returned,
and **no handle completed**. Its submitter parks on that handle for the rest
of the boot.

The window is not a rare interleaving, it is the busy one. The first pass's
`retire_op` calls complete every waiter with `BlockError::Io`, and
`block_io::read_blocking` re-issues immediately from those very threads --
straight back into the gap the first pass has just walked past.

## What made it invisible

The lost command is invisible to every later watchdog sweep, so nothing ever
notices. `watchdog_sweep` counts an op as hung only if it is still installed
in `cmd_slots` *and* in `OP_PENDING`; this one is in neither, because it was
dropped. With nothing hung the sweep returns silently, and the serial log
stops even though the kernel is fine.

That silence is what the earlier writeup read as "the guest is alive and
nothing is runnable". It was half right.

## The reading that settled it

Nothing in the log distinguishes a wedged kernel from a quiet one, so the
kernel's own globals were read out of the running guest over QMP instead --
`nm` for the address, `x /1gx` for the value, twice, two seconds apart.
`scripts/wedge-probe` now does this on every failure. Three wedges gave the
same three numbers:

| symbol | t0 | t1 (+2 s) | reading |
|---|---|---|---|
| `SWITCHES` | 0x33ac | 0x3bb9 | **+2061 switches in 2 s** |
| `NVME_INFLIGHT` | 1 | 1 | one command in flight, forever |
| `WATCHDOG_RESETS` | frozen | frozen | no sweep has fired since |

The first line is the one that matters, and it **refutes the standing
description of shape A**: the machine is not deadlocked with nothing runnable.
It runs about a thousand threads a second. That is the watchdog itself,
sleeping its millisecond and finding nothing hung, over and over, while the
`fs` kthread waits on a handle nobody holds any more.

An in-kernel stall detector was built first and never fired, which was the
correct answer to the wrong question: there was no stall.

## The fix

A slot is only ever emptied by **taking** its op and retiring it, looped until
a pass finds nothing new. `retire_op` returns without touching anything if the
op is already terminal, so the case the old second pass existed for -- a slot
whose op lost the reclaim race -- costs one no-op call rather than a stranded
buffer.

`/proc/nvme_stats` gained `installed_during_reset`, which counts what that
loop finds. It read **2632 in a single hostile boot**: the window is entered
constantly, which is why a defect in it was worth roughly one boot in five.

Two other counters went in with it and have stayed zero, so the shapes they
would catch are refuted rather than assumed: `slot_overwritten` (a reset
handing a submitter's command id to a second submitter, which would clobber a
live slot) and `cid_not_held_at_install`.

## What is left

One failure in ten still occurs, and it is **not** this shape:

| symbol | t0 | t1 |
|---|---|---|
| `NVME_INFLIGHT` | 0 | 0 |
| `IDLE_CPU_MASK` | 0xe | 0xd |
| `SWITCHES` | 0xfd31 | 0x10dae |

Nothing is outstanding, a CPU is busy, the busy CPU changes between samples,
and the boot has reached devfs registration -- much further than this bug ever
allowed. A guest under a controller resetting a thousand times a second is
also legitimately slow, and `doc/WORKING-NOTES.md` already says no correctness
fix changes that, so the next question is whether twelve seconds without a
serial byte is a defect at all or the probe's silence threshold meeting a
boot that is merely crawling. Measure before assuming a third bug.

## The lesson

**A slot that is cleared rather than emptied is a leak with no reporter.**
Every path that removes a command from the slot table has to hand it to the
one reclaim sequence, and the way to make that true is to take the value out
under the lock, so there is no version of the code that overwrites a live one.

And the general one, which cost this bug most of its life: **when a log goes
quiet, read the kernel's counters out of the running guest rather than
reasoning about the silence.** `nm` plus `x /1gx` over QMP needs no kernel
change, no rebuild, and no luck, and one pair of samples separated the whole
question -- deadlocked or live-locked -- that three sessions of reasoning had
got backwards.
