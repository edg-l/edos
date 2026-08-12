# `sync` that left the journal needing replay

## Status

Fixed by `6a15410`. Follow-up open: `sys_sync` still occasionally logs
`journal still pending after 8 rounds` even when the resulting image is
fsck-clean. The bound is doing its job, but the loop takes more rounds than it
should and nobody has explained why.

## Symptoms

After a clean `sync` with nothing else running, `efs-fsck` found a committed,
un-checkpointed transaction still in the ring. Mounting that image replays it.

Replay of a transaction whose blocks were *already* checkpointed and since
overwritten is not harmless: it reverts good data to the journal's older copy.

## Root cause

`sys_sync` ran a fixed two rounds of commit-then-flush. That is not a fixed
point. A flush pass writes file data out and enrols the metadata that maps it
into the journal's active transaction, and writeback refuses to check point a
block whose transaction has not committed — so **every round creates work for
the next one**. Two rounds simply moved the residue one round later.

It now loops until no journal reports committed work outstanding, bounded at 8
rounds, and logs when it hits the cap.

Two things had to be right for that loop to terminate at all:

- **It tests only committed work** (`sealed` plus `committed_pending`). The
  open transaction is refilled by every flush and is never replayed, so a
  condition that counts it never goes false.
- **It advances the tail inside the loop.** `committed_pending` is drained by
  `advance_tail` and by nothing else, so testing before that call can never go
  false either.

## The checker was wrong too

`efs-fsck` called a journal dirty when `tail_seq != head_seq`. That is not a
dirtiness test: `head_seq` names the *open* transaction, so a perfectly clean
journal normally sits one apart. The checker gained a ring scan and reported
dirty only when that scan found a committed transaction to replay.

That fix was half applied and stayed that way until 2026-08-12: the ring scan
ran **behind** the old `tail_seq != head_seq` test rather than in place of it, so
a head that had not caught up still suppressed the scan entirely, and a crash
between a commit and the superblock write hid committed work from the checker.
The head test is gone now, and the scan itself is shared with the kernel rather
than reimplemented; see `doc/efs.md` §14 and `doc/WORKING-NOTES.md`.

That fix is what turned this from a suspected false positive into a
demonstrated bug — the first instinct was to disbelieve the checker, and the
checker was wrong in a way that happened to be reporting the truth.

## Reasoning rules going forward

- **A drain loop whose work function creates work needs a fixed point, not a
  round count.** Any fixed count is a guess about how much residue one round
  leaves.
- **Bound the loop and log the cap.** A converging loop that silently fails to
  converge is worse than one that says so.
- **A dirtiness predicate must name the state it means.** `tail != head` was a
  proxy for "there is something to replay" and differed from it by exactly the
  common case.

## How to catch a recurrence

```bash
# boot, write a load, then from the guest:
sync
scripts/edos-vm stop
tools/efs-fsck/target/release/efs-fsck sata-disk.img
```

A clean `sync` must leave the journal reporting nothing to replay. Grep
`run_log.txt` for `sys_sync: journal still pending` — that line means `sync`
returned with a committed transaction un-checkpointed, and the next mount will
replay it.
