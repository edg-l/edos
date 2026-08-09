# Writes that never reach the disk

## Status

Fixed. Found while building `edos-install`, but none of it is specific to
the installer: every one of these is reachable from ordinary file I/O and
several predate it by months. The installer only made them certain, because
it writes ~75 MB through both filesystems in one burst and then reboots,
which is exactly the shape that turns "usually flushed eventually" into
"silently lost".

## Symptoms

Two families, both silent. Nothing panicked, no error reached userspace,
and `efs-fsck` reported the filesystem clean.

1. **Data loss.** A file read back correctly for as long as it stayed in
   the page cache and was zeros, or stale, after a reboot. The giveaway
   was that the cold-read hash of a 743104-byte file equalled the SHA-256
   of 743104 zero bytes: right size, right metadata, no data.
2. **Livelock.** A sustained write wedged the system. `journal_committer:
   seal_and_commit error: IoError` and `block_page_cache: detached
   fallback` repeated forever; a plain `cp` of a 40 MB file logged 17983
   commit failures. The desktop kept running until it needed the disk.

## Root cause

There is no single bug here. There is one design seam, crossed in nine
places.

**The seam: two caches, and paths that bypass one of them.** File data
lives in the per-inode page cache; blocks live in the block page cache.
Both filesystems write file data *directly to the device*, deliberately,
to avoid caching it twice. That is a sound choice, but it means a direct
write and a cached copy of the same block can disagree, and the cache
wins whenever it flushes afterwards.

- **Stale block-cache pages overwrote direct writes.** `EfsDriver` and
  `Fatfs` wrote file data with `block_write` / `submit_write` while the
  block page cache still held those blocks from the format or from a
  directory read. A later writeback pass put the old copy back on top.
  Both now invalidate the affected pages right after the direct write.
- **A page the cache had no room for was dropped.** When a shard was full
  of pinned or dirty pages, `insert_or_resolve_race` returned a *detached*
  page: usable, but not in the LRU, so writeback could never find it.
  Writes to it were discarded. Writers now drain and retry, and report
  the page's residency so a detached page is never treated as cached.
- **A forced flush could be answered by an unforced pass.** The periodic
  writeback tick shared `flush_requested`/`flush_completed` with explicit
  kicks, so a `sync` could be satisfied by a pass that deliberately skips
  recently-dirtied pages. The counters now track kicks only.
- **The writeback pass drained the caches in the wrong order.** Flushing
  an inode page writes *through* the block cache, so it creates block-cache
  dirt; draining blocks first left that behind. The pass is now
  blocks → inodes → blocks.
- **Dirty inodes were held by weak reference.** Closing the last
  descriptor freed pages that had never been written. The writeback list
  now pins them and releases the pin once they are clean.
- **Pages became flushable before the size that describes them.**
  `page_cache_write_core` marked pages dirty and only then stamped the new
  size, so a pass in that window saw a dirty page past EOF, wrote nothing,
  and cleared the dirty flag. Reliably the first page of a new file. The
  size is now published first.
- **A full journal ring was an error instead of a checkpoint.** Commits
  failed while the only thing that frees ring space is checkpointing, and
  writeback refuses to check point blocks of an uncommitted transaction:
  a closed loop. Commit now checkpoints and retries, and transactions are
  capped at half the ring so one can always fit.
- **The journal tail was pinned by blocks already on disk.** Every path
  that writes an enrolled block home must say so; the ones that bypassed
  the tracked writeback path did not, so `advance_tail` never moved. Those
  paths now report the checkpoint, and `sync` persists the tail it earned
  (a stale tail makes the next mount replay transactions whose blocks have
  since been overwritten, reverting good data).
- **The journal wrote its ring at partition-relative LBAs.** `Journal` was
  handed a block number counted from the start of the partition and used it
  as an absolute LBA, so on any partition not starting at LBA 0 the ring and
  the superblock landed ahead of the partition, on top of file data, while
  the mount kept reading the superblock at the correct address. It stayed
  hidden because the kernel only rewrites that superblock when the tail
  advances, and the tail never advanced until the fixes above; the first
  successful `advance_tail` is what exposed it. The journal now adds the
  partition's starting LBA.

## Reasoning rules going forward

- **A write that bypasses a cache must invalidate it.** Not "should not
  overlap" -- must invalidate. Both filesystems write file data directly
  for good reasons; that makes invalidation part of the contract, not an
  optimization.
- **Dropping a page is only safe when it is clean.** Eviction, detachment
  and device invalidation all have to check, and the check must be at the
  point of the drop, not in the caller.
- **A flush that says "nothing to write" must not clear the dirty flag**
  unless it is certain nothing was there. Both places where this bit us
  computed emptiness from a size that had not been published yet.
- **Order the writeback pass by which cache feeds which.** Upper layers
  dirty lower ones, never the reverse, so the lower layer drains last.
- **A durability wait must be answered by a durability pass.** Sharing a
  sequence counter between best-effort and forced work makes `sync` a lie.
- **A full journal is a checkpoint request, not an error.** Anything that
  returns an error there deadlocks against the only mechanism that could
  clear the condition.
- **A block number is meaningless without its frame of reference.** The
  journal's was partition-relative and its consumer treated it as absolute.
  Name the unit in the field, and convert at one boundary.

## How to catch a recurrence

The cheap check is a cold read. Copy a file, `sync`, reboot, and compare
hashes; comparing in the same boot proves nothing, because it reads the
page cache. If the cold hash equals the SHA-256 of that many zero bytes,
the data never left memory:

    python3 -c "import hashlib;print(hashlib.sha256(b'\0'*SIZE).hexdigest())"

`scripts/fs-regression` does exactly this and is the regression test for
everything here: it writes a set of files on a scratch partition, reboots,
and verifies them from a cold cache. `--fat32` runs the same check against
FAT32. It needs a normal ISO, so run `make all` first -- `make test` leaves
a `sched-test` build behind, which boots straight into the suite.

For the livelock, `grep -c 'seal_and_commit error' run_log.txt` after a
40 MB `cp` should be 0. A nonzero `detached fallback` count is not fatal
by itself, but a count in the thousands means the cache is thrashing and
something upstream is not draining.
