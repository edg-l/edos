# EFS lost bitmap bits and lost extents

## Status

Fixed by `3375ac4` (inode) and `6a15410` (bitmap). Two independent
unsynchronized read-modify-writes of shared on-disk structures, found in the
same session and by the same tool. Nothing here is specific to a workload; both
need only two threads touching one filesystem.

## Symptoms

Silent. Nothing panicked, no error reached userspace, and the filesystem stayed
mountable.

`efs-fsck` after a `fsbench /var` run reported **18721 leaked blocks** — bits
set in the allocation bitmap with no inode referencing them — plus "missing
bit" findings, which are the same race landing the other way round.

The inode half has no fsck signature of its own: a file simply comes up short,
because an extent one thread appended was overwritten by another thread's copy
of the inode.

## Root cause

**The bitmap.** `alloc_block` reads a bitmap block, sets a bit and writes the
block back, and took `alloc_mutex` to do it. `free_block` did exactly the same
thing to *clear* a bit and took **no lock at all**. `alloc_inode` and
`free_inode` were the same pair. The mutex's own comment only ever considered
two allocators racing each other, so the free side never looked like it was
missing anything.

With an allocation and a free interleaved on the same bitmap block, whichever
writes second restores the other's bit:

- a freed bit that stays set is a **leaked block**;
- an allocated bit that gets cleared is a block **handed out twice**.

Both appeared in the fsck output. The lock is now `bitmap_mutex`
(`RANK_EFS_BITMAP`, 32) and is taken on every bitmap read-modify-write, not
just the allocating ones.

**The inode.** The 256-byte on-disk inode is rewritten whole by more than one
caller: `update_size` stamps a new size, `ensure_block_for_logical` and
`ensure_blocks_for_logical_batch` append extents. Each reads the struct,
modifies its own field and writes all 256 bytes back, so a concurrent pair
loses whichever change was read first. Serialized by `inode_rmw`
(`RANK_EFS_INODE_RMW`, 31), which sits between `inode.lock` (30) and
`bitmap_mutex` (32). It is not reentrant, so the callers that already hold it
go through `_locked` inner variants.

## Two things this was blamed on and is not

Both were ruled out with `/proc/efs_stats`, which was added for exactly this
and exposes `blocks_allocated`/`freed`, `alloc_failed`, `tx_aborts` and
`orphans_marked`/`dropped`.

- **Not the tx-abort path.** `tx_aborts` is 0 across a whole run. The comment
  in `ensure_blocks_for_logical_batch` about `alloc_block` writing the bitmap
  before its transaction commits describes a real hazard that never fires.
- **Not orphan eviction.** `orphans_marked` and `orphans_dropped` both read 513
  after a run.

## Reasoning rules going forward

- **A lock on a read-modify-write must cover every writer, not every
  allocator.** The asymmetry here was invisible because the mutex was named and
  commented for the allocation path. If a structure is read-modify-written,
  name the lock after the structure.
- **A whole-struct write is a read-modify-write** even when the caller only
  means to change one field. Every path that rewrites the 256-byte inode is a
  writer of every field in it.
- **Counters before hypotheses.** Two plausible mechanisms were blamed here and
  both were refuted by a counter that took minutes to add.

## How to catch a recurrence

```bash
make clean-sata && make sata-disk.img     # an aged disk carries earlier runs' leaks
# boot, run: fsbench /var
scripts/edos-vm stop
tools/efs-fsck/target/release/efs-fsck sata-disk.img
```

A clean run reports 0 leaked, 0 missing, 0 orphans. The reference run allocated
309074 blocks in 75 seconds and reported zero findings.

Read `/proc/efs_stats` **after the benchmark process exits**, not from its own
report: fsbench prints its counter deltas before closing its descriptors, so
`orphans_dropped` reads 0 mid-run and 513 afterwards, which is how the orphan
path got blamed in the first place.
