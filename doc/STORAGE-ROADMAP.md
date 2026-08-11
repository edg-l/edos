# Storage roadmap

What to do next, in priority order, with the evidence for each. Measure with
`fsbench` before and after; `doc/fsbench.md` holds the record of what has
already been done and what the numbers mean, and `doc/bugs/` holds the
post-mortems for what broke on the way.

**The `/var` suite cannot resolve a small change.** Its metadata and warm-read
numbers swing by a factor of three between boots of one unchanged binary: three
fresh-disk runs of the same kernel gave `stat` 3291, 3574 and 10840 ops/s, and
`readdir` 13669, 19107 and 45423. Two runs are not a comparison. `fsbench raw`
is the stable one — 4 KiB p50 held at 104-105 us across four runs and two
builds — so anything below a few tens of percent has to be measured there, or
with many repetitions. Also reformat between arms (`make clean-sata && make
sata-disk.img`): `make all` only rebuilds the image when `filesystem/` changes,
so successive suites otherwise run against a disk carrying the previous runs'
files and leaked inodes. And read `/proc/efs_stats` **after** the benchmark
process exits — fsbench prints its counter deltas before closing its
descriptors, so `orphans_dropped` reads 0 mid-run and 513 afterwards.

**Every number below predates the monotonic clock moving off the HPET
(2026-08-11), and the per-command ones are wrong because of it.** An
`Instant::now()` cost 6361 ns then and 16 ns now, and a context switch went
20818 -> 1357 ns, and 433 ns after the round that followed it. Section 1 attributes ~100 us per 4 KiB command to two
scheduler round trips, so on the order of 40 us of that was clock reads. What
this actually moved, measured on the raw device: 4 KiB reads 30.4 -> 36.4 MiB/s
(p50 107 -> 92 us) and 512 B reads 13.6 -> 35.3 MiB/s (p50 9 -> 1 us, most of
the old figure being the benchmark's own two clock reads per operation).
Re-measure before trusting any absolute number here, and use `fsbench -n` so
both arms do the same work.

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

**Those two scheduler round trips are now the shared boundary with the context
switch work** (`doc/SCHED-ROADMAP.md`). They cost 20818 ns each when the 100 us
figure was measured and 285 ns now, which is where the 4 KiB gain since came
from. That lever is close to spent: what is left in a switch is 91 ns of
`fxsave` and `fxrstor` and a per-process TLB flush of about 108 ns, which needs
PCID this machine's CPU does not have. So depth is not merely the half
only this section can fix — it is the half with anything left in it.

The fix that matters is giving readahead and writeback a path that keeps
commands outstanding, so depth 32 is reachable at all. Until something asks for
depth, per-command micro-optimisation has nothing to bite on: see the two
completion-side cuts in the refuted list below, both of which were neutral.

Coalescing cannot help here — one page is one command — so this is
per-operation work, not another run-length fix.

## 1b. Pipelined readahead has no instrument that still shows a cost

The pipelined-readahead idea is to fire the *next* window's I/O when the reader
touches the last page of the current one, so the prefetch pulls ahead of the
reader instead of trailing it. The number quoted for it was `mmaptest` test 10
on `/var`, at about 500 ms on a first run.

**Measured 2026-08-12 on a cold boot, that test is 12 ms** — `fs::copy` of
`/bin/echo` 11 ms, `spawn+wait` 356 us, whole suite `mmaptest /var` 11/11 in
37 ms. The gain came from two things that landed since: whole-file prefetch
(`RA_WHOLE_FILE_MAX_PAGES`, 512 pages) turns a first sequential read of any file
under 2 MiB into one bulk fill, and `/bin/echo` is 329240 bytes, so test 10
never rides the ramping window at all. The old figure predates both and should
not be quoted again.

So the idea is not refuted, but nothing measures it. What survives is the
large-file case, above the whole-file threshold, where `page_cache_read_core`
(`fs/vfs.rs`) still extends only `window_size` pages past each request and
submits from within the read call. Before writing any of it, build the
instrument: a **cold** sequential read of a file several times
`RA_MAX_PAGES` (512 KiB) — 8-16 MiB — on `/var`, reboot between arms, and
watch `ahci_stats.ncq_max_inflight`, which is the direct read on whether the
prefetch is ahead of the reader or behind it. A change that does not move that
counter off 1 has not pipelined anything.

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

## 6. Creating a file walks the path three times

POSIX `EEXIST` on create is enforced in `vfs::create_file`/`create_dir`/
`symlink`, the one layer that already holds the parent inode write lock across
the create. The check costs `fs.file_info()`, plus `fs.read_link()` when that
misses, and both bypass the dentry cache. Stacked on `open_resolved`'s own
`file_info`, an `O_CREAT` open of a file that does not exist now walks the path
three times where one would do.

This is a real cost, not a bug, and it buys correctness that no filesystem
provided before: all three used to append a second directory entry with the same
name. It has not been measured. If `fsbench` metadata shows it, the fix is to
let each filesystem report `AlreadyExists` from its own `create_file` — it
already holds the parent — and delete the VFS pre-check, rather than threading a
lookup result down through the trait.

## Correctness items still open

Not performance, but found by this work and unfixed.

- **An intermittent segfault in `mmap store 4MiB + msync`**, roughly one
  `fsbench /var` run in six: `KILL: PF addr=... User write to unmapped page`.
  Unexplained. Item 4 above records a past `MAP_SHARED` segfault near the
  batch-allocation path, which is the first place to look.
- **`sys_sync` sometimes logs `journal still pending after 8 rounds`** even
  though the resulting image is fsck-clean. The bound is doing its job, but the
  loop takes more rounds to converge than the mechanism suggests it should
  (`kernel/src/syscalls/io.rs`).
- **A full filesystem is indistinguishable from an I/O error.** `alloc_block`
  returns `Error::IoError` when no group has a free block; `fs::Error` has no
  no-space variant, so userspace cannot report ENOSPC. `/proc/efs_stats`
  `alloc_failed` counts the case.
- **`AhciPort::mmio_lock` is a bare `spin::Mutex`** (`drivers/ahci/port.rs`),
  which CLAUDE.md rules out for state shared between threads: a preempted
  holder makes every other CPU spin. The hold is two MMIO writes, so this is
  latent rather than live.
- **A blank 1 GB disk once reported `/dev/sda holds 197120 bytes`** to
  userspace, while a 5 GB image on the same path reported correctly. Seen once,
  not reproduced deliberately. Worth ten minutes on `BlockDevNode::size` and the
  IDENTIFY path before trusting small-disk sizes.

## Recently closed

Kept as an index; the mechanism is in the post-mortem or the spec named on each
line.

- **Both storage regressions now run from one command.** `make storage-check`
  drives `scripts/fs-regression` (EFS then FAT32) and `scripts/fsbench-run`,
  which share their VM-driving helpers in `scripts/vmdrive.py`
  (`doc/vm-control.md`).
- **`fsync` could panic the kernel** — a wait predicate that took a
  `BlockingMutex`, evaluated inside `without_interrupts` (`8992a30`,
  `doc/bugs/2026-08-09-fsync-panicked-on-a-wait-predicate.md`).
- **A file could not have more than 13 fragments.** `MAX_INLINE_EXTENTS` is 13
  and that flat inline list was the whole block map, so a moderately fragmented
  1 MiB file failed `fsync` with `Unsupported` while writeback dropped the data.
  Depth-1 extent trees raise the ceiling to 4420 extents (`072106b`,
  `doc/efs.md` §6.4).
- **~19k leaked blocks per run** — an unsynchronized read-modify-write of the
  allocation bitmap, plus a second one on the whole inode (`6a15410`, `3375ac4`,
  `doc/bugs/2026-08-09-efs-lost-bitmap-and-inode-updates.md`).
- **`sync` left the journal needing replay** — a fixed two rounds of
  commit-then-flush is not a fixed point (`6a15410`,
  `doc/bugs/2026-08-09-sync-that-left-the-journal-dirty.md`).
- **A command could be completed before its data landed.** `on_port_irq` read
  SACT once and then walked the slots, so a command issued between the two
  reads paired with a clear bit; `issued` does not close the window, since the
  submitter stores it after writing SACT. `complete_ncq_slot` now re-reads SACT
  once it has observed `issued` (`9fb3af5`).
- **The reaper discarded evictions it could not queue**, leaking that inode's
  blocks until `efs-fsck` ran — several hundred per `fsbench /var` run. The
  reaper now parks the request on an unbounded overflow list (`EVICT_OVERFLOW`,
  rank 350) which the kthread drains ahead of the ring, so there is no capacity
  cliff to tune. `EVICT_DROPPED_COUNT` survives as queue-pressure telemetry; it
  no longer counts leaks (`bf669f6`).

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
