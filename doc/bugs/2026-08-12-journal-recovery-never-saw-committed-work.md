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

## The second bug: the partition offset was added twice

Fixing the scan exposed a second defect, unreachable before because nothing was
ever applied. Enrolment stamps a `DescriptorEntry` with the block page cache's
page index, which EFS derives as `block_to_lba(block) / SECTORS_PER_BLOCK` and
which therefore already carries the partition offset; the field is documented as
absolute. Pass 2 added `partition_start_lba` to it a second time, putting every
home write `partition_start_lba / SECTORS_PER_BLOCK` blocks too high. On a
partition starting at 1 MiB that is 256 blocks, so inode-table content landed on
the first data block, which is the root directory's, and recovery turned a
repairable filesystem into one whose root could not be read.

The ring read is the other addressing domain and *does* need the offset, since
`first_block` is partition-relative. That asymmetry inside one function is why
this survived the earlier fix to the same file.

What made it legible was a table, not a theory. Subtracting 256 from every
`fs_block` in the descriptor landed each one exactly on the structure whose
content the journal was carrying:

| journal says | -256 | what lives there | journal's data |
|---|---|---|---|
| 257 | 1 | superblock | - |
| 261 | 5 | block bitmap | a bitmap |
| 263-266 | 7-10 | inode table | inodes |
| 519 | 263 | root directory data | a directory |

Verified end to end: after the fix, replay's changed-block set moves from
`{257,258,261,262,263,264,265,266,519}` to `{6,7,10,263,...}`, `efs-fsck` no
longer reports the corrupt root entry, and a guest that is power-cut mid-write
comes back with its fsync'd file readable.

## Still open: orphaned inodes

A crash cycle still leaves inodes allocated with no directory entry naming them
(`orphan inode N`, which `efs-fsck --repair` can reclaim). These are inodes from
transactions that never committed. Worth confirming whether `EfsDriver::write_block`
putting the page to its home location before the transaction commits breaks
write-ahead ordering, or whether that write lands in the journal-gated block page
cache and is harmless. That is the next question in this subsystem.

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
