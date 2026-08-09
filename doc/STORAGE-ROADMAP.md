# Storage roadmap

What to do next, in priority order, with the evidence for each. Measure with
`fsbench` before and after; `doc/fsbench.md` holds the record of what has
already been done and what the numbers mean.

**The `/var` suite cannot resolve a small change.** Its metadata and warm-read
numbers swing by a factor of three between boots of one unchanged binary: three
fresh-disk runs of the same kernel gave `stat` 3291, 3574 and 10840 ops/s, and
`readdir` 13669, 19107 and 45423. Two runs are not a comparison. `fsbench raw`
is the stable one — 4 KiB p50 held at 104-105 us across four runs and two
builds — so anything below a few tens of percent has to be measured there, or
with many repetitions. Also reformat between arms (`make clean-sata && make
sata-disk.img`): `make all` only rebuilds the image when `filesystem/` changes,
so successive suites otherwise run against a disk carrying the previous runs'
files and leaked inodes.

Where things stand as of 2026-08-09:

| | Measured | Against |
|---|---|---|
| raw device read, 1 MiB | 886 MiB/s | 1130 MiB/s over a ramdisk |
| raw device write, 1 MiB | 543 MiB/s | preallocated image; 280 on a sparse one |
| file read, 1 MiB, warm | 1766 MiB/s | memory speed |
| buffered file write, 1 MiB | 318-425 MiB/s | page cache, not the disk |
| durable write, 1 MiB + fsync | 28-55 MiB/s | |
| 4 KiB, read or write | ~30 MiB/s | ~100 us per command |
| mmap fault | 121 us per page | 0.5 us per page for `read` |
| `edos-install`, blank 5 GB disk | 2.8 s | was 4.3 s |

**Sequential bandwidth is finished.** 886 MiB/s read and 543 MiB/s write sit
either side of SATA III's 600 MB/s line rate, so on real hardware there is
nothing left to win there. Everything below is per-operation cost.

## 1. Nothing ever queues more than one command

4 KiB access costs about 100 us per command on both read and write, capping the
system near 10k IOPS where a real SATA SSD does 50-100k at queue depth 32. This
is the one remaining number that would still matter on real hardware.

The reason is structural, not a slow submit path. **Every I/O path in the kernel
but one is submit-then-wait.** `block_read` / `block_write` / `block_write_fua`
in `fs/efs/mod.rs`, `read_frame` / `write_frame` / `write_frames` in
`fs/block_page_cache.rs`, and the equivalents in `fs/journal/`, `fs/fat32/`,
`fs/gpt.rs` and `fs/mbr.rs` all call `submit_*` and immediately park on the
handle. EFS `flush_pages_bulk` coalesces into runs but still blocks on each run.

The single exception is `BlockPageCache::read_pages` → `submit_read_batch`, and
it coalesces contiguous misses first, so a 1 MiB miss is two commands rather
than 256. `fsbench /var` reports `ahci_stats.ncq_max_inflight +1`: the whole
suite never has two commands outstanding.

So a 4 KiB read is a dependent round trip, not a queued one, and the 100 us is
what a round trip costs: syscall, cache lookup, frame allocation, submit, park,
device, MSI, wake the dispatcher kthread, complete, wake the submitter — two
scheduler round trips per 4 KiB. Anything that only shortens submission is
attacking the cheaper half.

The fix that matters is giving readahead and writeback a path that keeps
commands outstanding, so depth 32 is reachable at all. Until something asks for
depth, per-command micro-optimisation has nothing to bite on: see the two
completion-side cuts in the refuted list below, both of which were neutral.

Coalescing cannot help here — one page is one command — so this is
per-operation work, not another run-length fix.

An unrelated defect found while reading the completion path: `on_port_irq`
reads `SACT` and then loads each slot's op, so a command issued between those
two reads is completed by the dispatcher before its data has landed. `issued`
does not close the window, since the submitter stores it after writing SACT.
Narrow and never observed, but real; re-reading `SACT` before completing a slot
would close it.

## 1b. `fsync` could panic the kernel (FIXED)

`Journal::committed_seq` took the `BlockingMutex<JournalState>`, and
`force_commit_and_wait` handed `|| self.committed_seq() >= target_seq` to
`commit_wq.wait_until_timeout`. `WaitQueue::wait_internal` evaluates its
readiness predicate inside `without_interrupts`, and `BlockingMutex::lock`
debug-asserts interrupts are enabled when it has to block. So an `fsync` that
re-checked its predicate while the committer kthread held `state` panicked the
kernel; the CPU then stopped acknowledging IPIs and `tlb_shootdown` panicked on
top of it. Caught once during a `fsbench /var` run on trunk — racy, not
deterministic, which is why the suite usually completed.

Two more call sites had the same shape: the committer kthread's own
`has_pending_work()` predicate, and `Mailbox::recv`, which re-checked its
`BlockingMutex<VecDeque>` under the same interrupts-off rule.

Fixed by making every wait predicate lock-free rather than by relaxing the
assert. `committed_seq` is now mirrored into an `AtomicU64` published under the
state lock by `set_committed_seq`, so the field and its mirror cannot drift;
the committer uses a `try_lock` hint biased towards "there is work"; `Mailbox`
reuses the `try_lock`-based `is_empty` it already had. The requirement is
documented on `WaitQueue::wait_until`.

## 1c. A file cannot have more than 13 fragments

`MAX_INLINE_EXTENTS` is `(INODE_DATA_AREA_SIZE - sizeof(EfsExtentHeader)) /
sizeof(EfsExtent)` = `(176 - 12) / 12` = **13**. Every file's block map is that
flat list, inline in the inode. Once a file needs a fourteenth discontiguous
run, `ensure_block_for_logical` and `ensure_blocks_for_logical_batch` fail the
`extents.push` and return `Error::Unsupported` (`fs/efs/mod.rs:915`, `:1057`).

Caught by `fsbench /var -t 4000`, which fails `write 1MiB + fsync each` with

```
sys_fsync: flush_file(/var/fsbench.fsync) error: Unsupported
```

once the suite's earlier phases have fragmented the free space. A 1 MiB file is
256 blocks, so it only takes moderate fragmentation. The same call sits on the
writeback path, so this is not only a failing `fsync`: the data never reaches
disk.

The format already anticipates the fix. `EfsExtentHeader` carries a `depth`
field, `EfsExtentIndex` is defined for internal nodes, and the reader rejects
`depth != 0` with "v1 only supports depth-0" (`fs/efs/mod.rs:466`). Implementing
depth-1 raises the ceiling to 13 index entries times whatever a 4 KiB leaf
block holds, which is far past anything reachable. It touches the extent
reader, both `ensure_block*` paths, truncate/free, `efs-fsck`, `efs-mkfs` and
`doc/efs.md` §extents.

Until then the ceiling is real and silent, and the allocator's contiguity
behaviour is what decides how often a file hits it.

## 1d. The block leak (FIXED)

`efs-fsck` reported ~19k leaked block-bitmap bits after a `fsbench /var` run:
bits set with no inode referencing them.

The cause was a read-modify-write race on the allocation bitmaps. `alloc_block`
reads a bitmap block, sets a bit and writes the block back, and took
`alloc_mutex` to do it. `free_block` did exactly the same thing to clear a bit
and took **no lock at all**; `alloc_inode` and `free_inode` were the same pair.
The mutex's comment only ever considered two allocators racing each other. With
an allocation and a free interleaved, whichever writes second restores the
other's bit: a freed bit that stays set is a leaked block, an allocated bit
that gets cleared is a block handed out twice. Both appeared in fsck output
("leaked" and "missing bit").

Renamed `bitmap_mutex` and taken on every bitmap read-modify-write. After a
75-second run allocating 309074 blocks, fsck reports **zero** findings — 0
leaked, 0 missing, 0 orphans — against 18721 leaked before.

Two things `/proc/efs_stats` ruled out on the way, both previously blamed here:

- **Not the tx-abort path.** `tx_aborts` is 0 across a run. The comment in
  `ensure_blocks_for_logical_batch` about `alloc_block` writing the bitmap
  before its transaction commits describes a real hazard that never fires.
- **Not orphan eviction.** `orphans_marked` and `orphans_dropped` both read 513
  after a run. Sampling them *during* the run shows 0 dropped, because the
  benchmark prints its counters before closing its descriptors — read them
  after the process exits or they say the opposite of the truth.

A second, independent race was fixed while looking: the whole-inode
read-modify-write in `update_size` against the one in
`ensure_block*_for_logical`, which loses extents rather than bitmap bits. See
`RANK_EFS_INODE_RMW`.

## 1e. `sync` left the journal needing replay (FIXED)

After a clean `sync`, `efs-fsck` found a committed, un-checkpointed transaction
still in the ring; a mount would replay it.

`sys_sync` ran a fixed two rounds of commit-then-flush. That is not a fixed
point: every checkpoint pass enrols the metadata mapping the data it just
wrote, so each round creates work for the next. It now loops until no journal
reports committed work outstanding, bounded at 8 rounds, and logs if it hits
the cap.

Two things had to be right for that loop to terminate. It tests only
*committed* work (`sealed` plus `committed_pending`): the open transaction is
refilled by every flush and is never replayed, so counting it never converges.
And it advances the tail *inside* the loop, because `committed_pending` is
drained by `advance_tail` and nothing else, so testing before that call can
never go false.

`efs-fsck` also grew an accurate dirtiness test. `tail_seq != head_seq` is not
one: `head_seq` names the open transaction, so a clean journal normally sits
one apart. It now scans the ring and reports dirty only when it finds a
committed transaction to replay, sharing that scan with replay itself. That is
what proved this was a real bug rather than the false positive it first looked
like.

## 2. mmap fault-around

A map, fault in 4 MiB, unmap cycle is 124 ms: 1024 pages at 121 us each,
against 527 us for a `read` of the same bytes. The pages are already in the
inode page cache during that test, so no device is involved — the cost is the
fault path itself.

The fix is mapping several PTEs per fault rather than one, so 1024 pages cost
far fewer than 1024 faults. That means changing `FaultOutcome` and the VMA
page-slot bookkeeping, which today carry exactly one page each. `munmap` of a
large mapping is on the same path and worth timing alongside it.

Measure a **cold** mmap first — `fsbench write`, reboot, `fsbench read` — so
fill cost and fault cost can be told apart rather than assumed.

## 3. Detached pages are a crash-consistency risk, not just a slow path

When a shard is full, `read_page_for_write` gives up after
`WRITE_PAGE_ATTEMPTS` and returns a detached page, and `publish_write` writes it
straight to its home location — ahead of the journal committing it. The comment
on `read_page_for_write` states the consequence: a replay after a crash
overwrites newer data with the journal's older copy.

Raw device writes no longer produce any (12562 -> 0, since whole-page runs
bypass the cache entirely), and the journal ring change cut the rest from 2626
to roughly 750 per benchmark run. Several hundred is still an escape hatch
firing routinely rather than exceptionally, and the 8 MiB cache (8 shards x 256
pages) remains small for the metadata working set. Size the cache to the
working set, and treat a detached write as something to alarm on rather than
absorb silently.

## 4. The allocating write path rewrites the inode per block

`write 512B` allocating runs at ~3.6 MiB/s against ~16 MiB/s overwriting the
same blocks. `ensure_block_for_logical` does a `read_inode`, an extent parse and
a `write_inode` for every 4 KiB block.

`ensure_blocks_for_logical_batch` already does the batched version, but it also
writes the inode itself, so it cannot simply be substituted — doing that
produced a segfault inside a `MAP_SHARED` mapping. Give it a mapping-only mode
whose caller owns the inode write.

## 5. Durable writes are far below buffered

28-55 MiB/s against 318-425 buffered. Some of that is inherent — a journal
commit, a FUA commit block and a drive cache flush per `fsync` — but real
hardware with FUA loses two to three times, not eight. Worth attributing across
those three before deciding there is nothing to take.

## 6. Small, cheap

- `scripts/fs-regression` still has to be run by hand after `make all`. Wire it
  into a make target alongside `fsbench`, so a durability regression and a
  throughput regression are caught by the same command.
- The evict queue holds 256 entries and fills during an ordinary create/unlink
  burst. The *synchronous fallback* arm is rate-limited and counted in
  `/proc/evict_stats`. The *reaper* arm used to discard the request outright,
  logging one unconditional line per inode and leaking that inode's blocks
  until `efs-fsck` ran; a single `fsbench /var` run emitted several hundred.
  Fixed: the reaper now parks the request on an unbounded overflow list
  (`EVICT_OVERFLOW`, rank 350) which the kthread drains ahead of the ring, so
  nothing is lost and there is no capacity cliff to tune.
  `EVICT_DROPPED_COUNT` survives as queue-pressure telemetry; it no longer
  counts leaks.
- A blank 1 GB disk once reported `/dev/sda holds 197120 bytes` to userspace,
  while a 5 GB image on the same path reported correctly. Seen once, not
  reproduced deliberately. Worth ten minutes on `BlockDevNode::size` and the
  IDENTIFY path before trusting small-disk sizes.

## What has been tried and did not work

Five experiments cost a build-and-boot each and are recorded so they are not
repeated. All of them came from reasoning that sounded right and was refuted by
measurement.

- **Boosting block-I/O completion wakes to `WakePriority::Interrupt`.** The
  premise was that a waiter released by a device completion has already paid
  the transfer latency and belongs ahead of threads that merely became
  runnable, matching what the legacy AHCI poll path already does for its own
  waiters. Neutral on the raw device. Reverted for lack of any demonstrated
  benefit against a real mechanism for harm: every block-I/O waiter includes
  writeback and the journal committer, so boosting them all starves whatever
  waits for NCQ to drain — the same shape as the `enter_legacy_mode`
  writer-preference experiment below.
- **Walking only allocated slots in `on_port_irq`** instead of all 32 twice.
  The premise was sound — an idle port pays 64 tracked lock acquisitions per
  IRQ pass to find nothing, against 2 on the whole submit path — but it is
  invisible in the numbers: raw 4 KiB p50 104.5 us before, 103.5 us after,
  inside a ±6% noise band. Kept anyway, since it is strictly less work with
  identical semantics, but it is not a throughput fix and should not be cited
  as one.

- **Coalescing `flush_dirty_once`.** The premise was that
  `checkpoint_and_advance` calls into it, so it must be what a slow commit is
  made of. `block_cache.writeback_bytes` is about 4 MB per run, a tenth of a
  second even at the old rate — the counter was already printing and refuted
  the idea before any code was written. Measured, it cut `stat` from 37444 to
  14731 ops/s and `readdir` from 258715 to 73754, because those dirty pages are
  metadata the rest of the system is actively using and holding a run of page
  locks across one large DMA stalls all of them.
- **Writer preference for `enter_legacy_mode`**, to stop a FLUSH CACHE starving
  behind NCQ traffic. Made `write 1MiB + fsync` go from 97 ms to 30 s.
- **Fill-ahead in the page fault handler.** Neutral (26.3 -> 28.6 MiB/s), and
  it first shipped a segfault: a bulk fill publishes failure for its whole
  range, so including the faulting page let a bulk failure kill the faulting
  thread.

The general rule they add up to: **coalescing pays where a path is
throughput-bound on a long run of contiguous blocks it owns exclusively** — raw
device I/O, EFS file data, the journal ring. It costs where the blocks are
small, scattered, or contended by other work. Check which one a path is before
reaching for it, and check the counters that would refute the premise first.
