# An AHCI port restart stranded the op it meant to fail

## Status

Fixed in `cf5c9821`. The reset generation is published in
`AhciPort::begin_restart` (`kernel/src/drivers/ahci/port.rs`), before the
fail-all pass, and the submitter re-reads it after storing `issued`.

## Symptoms

An NCQ op waits for a completion nobody will deliver after an AHCI port
restart. A watchdog sweep finds it up to 30 s later. On a real disk image this
never happens on its own: a command against qcow2 completes in well under a
millisecond, so no sane watchdog timeout fires (a 30 ms timeout fired zero
times under load).

## Root cause

`fail_all_ncq_slots` skips a slot whose `issued` is still false, trusting the
submitter to notice the generation change and complete its own slot. But
`reset_generation` was bumped at the end of `restart_port`, after that pass. A
submitter that stored `issued` between the pass and the bump, and sampled
`SACT` before the reset cleared it, saw an unchanged generation and its bit
still set. It returned and waited.

With the bump moved ahead of the pass, the two orderings are complementary:
either the submitter sees the bump and completes its own slot, or its store to
`issued` precedes the pass and the pass fails the op.

Gating `enter_ncq_mode` on `restarting` went in at the same time as a
throughput measure. It is not the fix: the stranded op is already past that
gate.

## Reasoning rules going forward

- A generation counter that tells a racing party "something changed" must be
  published before the pass that relies on that party noticing, not after.
- A race that real hardware timing never produces needs injection to test.
  Proving a fix by waiting for the natural rate proves nothing here.

## If this reappears

1. `/proc/ahci_stats` `stranded=` counts ops a sweep finds pending from an
   earlier generation, the exact fingerprint. Nonzero is this bug.
2. The log line is `ahci: stranded op port=... slot=... gen=... now=...`.
3. To force it: `ahci_ncq_timeout_ms=0` on the kernel command line
   (`kernel/src/drivers/ahci/watchdog.rs`) makes every sweep treat every
   in-flight op as hung, so restarts land inside submits at the I/O rate.
   Measured under mixed read/write with forced restarts: 1 stranded in 33
   restarts before the fix, 0 in 106 after. The knob is inert unless the
   command line sets it.
