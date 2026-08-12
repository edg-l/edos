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

Two hypotheses were raised and both were refuted by experiment; the record is
kept because each was plausible and re-deriving them wastes a session.

**Refuted: a home-block address error in pass 2.** Its addressing is correct,
`lba = partition_start_lba + fs_block * SECTORS_PER_BLOCK`, the same convention
the ring read uses.

**Refuted: replay dragging blocks backwards.** The theory was that
checkpointing had already advanced home blocks past the transaction replay
re-applied. Snapshotting the disk after the crash but *before* any mount kills
it. At that point `efs-fsck` reports:

```
[ERROR][dir-tree] orphan inode 2..11 (reachable link count 0)
(no block-bitmap errors at all)
```

The root directory is intact and ten inodes are orphaned: the inode table
reached disk, the directory entries naming those inodes did not. That is the
ordinary half-finished state the journal exists to repair, and nothing has
moved *forward* past the transaction being replayed. Running replay over it
then produces the corrupt root entry and 54 bitmap leaks.

**So replay damages a repairable filesystem, and the defect is in what it
writes rather than in when it writes it.** It wrote inode-table content into
the root directory's home block, which means the pairing between a descriptor's
list of block addresses and the data blocks following it is misaligned: entry
*i* named the directory block while `data_blocks[i]` held inode-table content.

### Partly addressed: the superblock now publishes on commit

`seal_and_commit` writes the journal superblock once a transaction's ring
blocks are down, rather than leaving that to `advance_tail`. Before, a whole
workload could run without the superblock being written at all: it kept the
values `efs-mkfs` wrote while the ring wrapped underneath it, so nothing on
disk described the ring's real extent. `efs-fsck` on a crashed image now
reports `journal is dirty ... tail_seq=1 head_seq=2` where it previously saw a
clean journal and said nothing.

The write was already FUA, so this costs one extra barrier per commit. That is
a real cost on a path where barrier count already dominates fsync latency
(`doc/STORAGE-ROADMAP.md` §1), and it is worth revisiting once the remaining
defect is understood; correctness first.

**It does not fix the corruption.** A crash-and-recover cycle still ends with
`dir inode 1: entry '' points to out-of-range inode` and leaked bitmap bits.
Three hypotheses have now been refuted by experiment (wrong home-block
addresses, replay dragging blocks backwards, and transaction granularity — the
EFS write paths already open one transaction per VFS operation and
`create_file_inner` enrols the inode, the inode table and the parent directory
entry in it). Do not re-derive them.

What is known and not yet explained: replay applies transaction 1 faithfully,
block 519 ends up byte-identical to the journal's copy of it and is a valid
directory block containing `.`, `..` and the iotest filenames, and yet the root
inode comes back unreachable. The next question is therefore not what replay
writes but **which block the root inode's extent points at after replay** — if
inode 1's extent does not name 519, `efs-fsck` is reading a different, stale
block and everything else follows. Dump inode 1 out of the replayed inode-table
block and compare its extent against 519 before touching any code.

Next step, on the host, against a pre-replay snapshot: parse the ring's
descriptor block, list its `fs_block` entries in order, and compare each
against the content of the data block replay would pair with it. Candidates for
where the misalignment enters, in order of suspicion: whether the writer places
the revoke block where the reader expects it, whether a descriptor's entry
count can exceed the data blocks actually written, and whether an escaped block
consumes a ring slot the reader does not account for. `doc/efs.md` §14 is the
format authority.

The snapshot-before-mount technique is what made this legible, and it is worth
reusing: an unclean image is evidence, and mounting it destroys the evidence.

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
