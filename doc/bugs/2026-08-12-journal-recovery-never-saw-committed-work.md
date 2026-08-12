# Recovery reported "clean" and discarded every committed transaction

**Found** 2026-08-12, immediately after the recovery-testing instrument landed.
**Severity** data loss on any unclean shutdown, reachable by any program, no
instrumentation required. `fsync` returned success for work that recovery then
threw away.

## Symptom

Mount a scratch EFS, run `iotest /mnt` (which calls `sync_all`), cut power five
seconds in, remount:

```
efs journal: clean, no replay needed
```

`ls /mnt` is empty. Every file created and fsync'd is gone, and recovery
believed there was nothing to recover.

## Mechanism

Three things compose into it, and each is reasonable alone.

1. `Journal::write_journal_sb()` had exactly one call site, inside
   `advance_tail`, gated on `changed`. The on-disk `JournalSuperblock`'s
   `head_seq` and `head_block` were therefore persisted only when the tail
   advanced, which happens only when checkpointing retires transactions.
2. `advance_tail` sets `tail_seq = head_seq` and `tail_block = head_block`
   whenever nothing is sealed or pending, then writes the superblock. A
   quiescent filesystem thus records **head == tail** on disk.
3. `replay()` early-returned on `head_seq == tail_seq` with "clean, no replay
   needed", and otherwise bounded its pass-1 scan by the persisted
   `head_block`.

So the window between a commit and the next `advance_tail` that retires it was
invisible to recovery: the superblock still said clean, and replay never
looked. The window is not exotic. It is every commit, from the moment it
becomes durable until writeback happens to checkpoint it.

## Why nothing caught it for so long

The note this file supersedes recorded that "every power cut mid-workload gave
`clean, no replay needed`", and read that as writeback checkpointing promptly
so there was genuinely nothing to replay. The correct reading is the opposite:
the superblock could never report anything else, because nothing updated it
between checkpoints. The observation that looked like evidence of health was
the symptom.

That misreading is the reusable lesson. `clean, no replay needed` is an
assertion about a *record*, not about the *ring*. Treating it as proof that the
ring was empty is how a recovery path stays broken through two separate
investigations.

`fs-regression` cannot see any of this: a clean unmount leaves nothing to
replay, so the suite passes on a kernel whose recovery does nothing at all.
This is the second bug in this area with that property; see
`2026-08-12-journal-replay-read-the-wrong-lba.md` for the first.

## Fix

Bound the pass-1 scan by **sequence continuity** rather than by the persisted
head, which is how JBD2 avoids needing a durable head at all. Replay now walks
forward from the persisted tail and accepts a transaction only while its `seq`
is exactly the next expected one and its commit block validates; the persisted
head is advisory. The `head_seq == tail_seq` early return is gone, so every
mount scans. That costs one block read on a genuinely clean journal, because
the first block fails the continuity check.

Sequence continuity is sufficient because transaction sequence numbers are
globally monotonic and never reused: no stale block left over from an earlier
trip around the ring can carry the sequence number the scan is looking for. It
also subsumes the fix in
`2026-08-12-journal-replay-read-the-wrong-lba.md`, which added the head bound
specifically to stop the scan from running into older transactions past the
head that still parse. Those transactions carry lower sequence numbers, so
continuity stops there too, without needing the head to be durable.

## Still open: replay writes home blocks to the wrong locations

Fixing the above exposed a second, independent defect, which had been
unreachable because replay never applied anything. The same reproducer now
replays and then fails `ls /mnt`. `efs-fsck` on the image says:

```
[ERROR][dir-tree] dir inode 1: entry '' points to out-of-range inode 562949953438189
[ERROR][block-bitmap] leaked block-bitmap bit at 264..317   (54 of them)
```

`562949953438189 - 2^49 = 16877 = 0o40755`, which is `S_IFDIR | 0755`: the
bytes parsed as a directory entry's inode number are an inode's **mode field**.

That first reads as a home-block address error, but pass 2's addressing was
checked and is correct — `lba = partition_start_lba + fs_block *
SECTORS_PER_BLOCK`, the same convention the ring read uses. The likelier
mechanism is a **rollback**, and it follows from how far the scan got:

Replay applied transaction 1 and stopped at the continuity break. But
checkpointing had been running normally during the workload, so later
transactions had already reached their home blocks before the crash. Writing
transaction 1's version of a block over a home location that had already
advanced to transaction 2's version moves that block *backwards*. The result on
disk is a mix of old and new metadata, which is exactly what a directory block
holding plausible-looking garbage looks like.

If that is right, the invariant being violated is: replaying transaction N is
only safe when every committed transaction after N is replayed too, because
checkpointing is in-order — if transaction 2's blocks reached home, transaction
1's did as well. A scan that stops early therefore cannot simply apply what it
found. Either the earlier transactions must be skipped when the persisted tail
is known to lag, or the scan must establish that it reached the true end of the
committed region before applying anything.

Confirm before fixing: instrument which home blocks pass 2 writes, and compare
against the block that `efs-fsck` reports as corrupt. If that block's pre-replay
content was *newer* than what replay wrote, the rollback hypothesis holds and
the addressing is a red herring.

Until that is fixed, recovery preserves committed work and lands some of it in
the wrong place. Both states are bad; neither is a release.

## Regression

`make recovery-check` runs the cycle unattended: format a small-ring scratch
disk, mount it in a guest, fsync a workload, cut power over QMP, reboot,
remount, and assert the fsync'd data survived.

**It has not been shown to fail against a broken kernel.** The agent that wrote
it was stopped mid-verification, so treat it as scaffolding rather than as a
passing test until someone has watched it go red against a kernel with the scan
fix reverted. A regression test for a bug this quiet is worth nothing if it
passes either way.

The wrapped-ring case is also still untested against the fix: both wrapped runs
recorded during the investigation predate it.
