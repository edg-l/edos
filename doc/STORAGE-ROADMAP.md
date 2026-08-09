# Storage roadmap

What to do next, in priority order, with the evidence for each. Measure with
`fsbench` before and after; `doc/fsbench.md` holds the record of what has
already been done and what the numbers mean.

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

## 1. Per-command cost — the next thing

4 KiB access costs about 100 us per command on both read and write, capping the
system near 10k IOPS where a real SATA SSD does 50-100k at queue depth 32. This
is the one remaining number that would still matter on real hardware.

`/proc/ahci_stats` says where it is not: `ncq_max_inflight` peaks at **9** out
of a negotiated 32 even when a batch hands the driver 64 commands at once. The
device is not the constraint — we cannot feed it. The cost is in getting a
command issued.

Candidates, in the order they appear in `submit_ncq_read`:

- `install_ncq_op`: an `Arc` allocation and an `owned_ops` push per command,
  plus the ranked lock on `ncq_waiters[slot]`.
- `enter_ncq_mode` / `exit_ncq_mode` around every submission.
- `issue_ncq_command`: MMIO register writes, each a VM exit under QEMU, plus
  the post-issue `read_volatile` of SACT.

Profile before cutting. The cheapest first step is to time the phases of one
submission and find which of the three owns the 100 us, rather than assuming.
Note the figure is measured on a dev-profile kernel, where the lock-order
tracker runs on every `ranked_lock!`; how much of the 100 us is that has never
been quantified, so treat 100 us as an upper bound on the real work.

Coalescing cannot help here — one page is one command — so this is
per-operation work, not another run-length fix.

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
  burst, falling back to synchronous eviction. The log flood is fixed and the
  fallback is counted in `/proc/evict_stats`; the capacity and the drain rate
  behind it are not.
- A blank 1 GB disk once reported `/dev/sda holds 197120 bytes` to userspace,
  while a 5 GB image on the same path reported correctly. Seen once, not
  reproduced deliberately. Worth ten minutes on `BlockDevNode::size` and the
  IDENTIFY path before trusting small-disk sizes.

## What has been tried and did not work

Three experiments cost a build-and-boot each and are recorded so they are not
repeated. All three came from reasoning that sounded right and was refuted by
measurement.

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
