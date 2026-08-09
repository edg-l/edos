# Storage roadmap

What is left after the 2026-08-09 fsbench round, ordered by what the numbers
say rather than by how interesting it is. Every item names the evidence that
motivates it; re-measure with `fsbench` before and after.

One pattern paid off three times in that round: **issuing a command costs far
more than the sectors it carries.** Coalescing contiguous blocks into one
command took raw device reads from 37 to 886 MiB/s, `sync()` after a write
phase from 28.7 s to 21 us, and `mmap store + msync` from 8.8 to 1059 MiB/s.

It is not a universal lever, and item 1 below is the counter-example: a fourth
application of it regressed the system and was reverted. Coalescing pays where
a path is throughput-bound on a long run of contiguous blocks it owns
exclusively. It costs where the blocks are small, scattered, and contended by
other work.

## 1. The journal commit (largely done)

A process's first `fsync` after a write phase that buffered hundreds of
megabytes cost 6-12 s, all of it inside `force_commit_and_wait`.

Most of it was `seal_and_commit` writing each enrolled block with its own
command. Coalescing the ring writes took that fsync to 1.6-2.5 s and the whole
suite from 20.6 s to 10.1 s, and dropped `detached_fallbacks` over a run from
2201 to 580 as a side effect: the ring frees sooner, so writers stop being
handed detached pages. Note this is the *opposite* result to the reverted
experiment below, and for the reason given there — the ring is contiguous and
exclusively owned, the block cache's dirty set is neither.

What is left of the commit cost is the 1.6-2.5 s that remains. Measure again
before assuming where it is.

**Coalescing `flush_dirty_once` was tried and reverted.** The reasoning was that
`checkpoint_and_advance` calls into it and it writes one command per 4 KiB page,
so it must be what the commit is made of. It is not: `block_cache.writeback_bytes`
is about 4 MB per run, which even at the old per-page rate is a tenth of a
second, nowhere near the 6-12 s being attributed. The number was sitting in the
counters the whole time and would have refuted the idea before any code was
written.

Measured, the change was a clear regression, because the dirty pages in this
cache are metadata that the rest of the system is actively reading and writing,
and holding a run of page `write_lock`s across one large DMA stalls all of them
for the length of the batch instead of one page at a time:

| | Before | Coalesced |
|---|---|---|
| `stat` | 37444 ops/s | 14731 |
| `readdir` | 258715 entries/s | 73754 |
| `sync()` after write phase | 21 us | 187 ms |
| `write 4KiB + fsync` | 12.25 s | 12.63 s |

The lesson generalises: coalescing pays where a path is throughput-bound on a
long run of contiguous blocks it owns exclusively (raw device reads, EFS file
data). It costs where the blocks are small, scattered, and contended. Check
which one a path is before reaching for it.

## 2. The install path (done: 4.3 s -> 2.8 s)

`edos-install` on a blank 5 GB disk was 4.3 s: 1.6 s formatting the root
filesystem, 2.3 s in the final flush, everything else under 0.2 s. Both of
those go through `BlockPageCache::write_bytes`, which staged every page through
the cache even when the write covered the page completely.

Sending a whole-page run straight to the device instead took raw sequential
writes at 1 MiB from 44 to 280 MiB/s, dropped `detached_fallbacks` over the
sweep from 12562 to zero, and took the install to **3.0 s** with the final
flush at 1.0 s.

Two things worth knowing from the measurement:

- The 64 KiB raw write reads *slower* afterwards (43 -> 32 MiB/s). That is the
  old number having been inflated by write-back deferral: the call returned as
  soon as the pages were dirty and the writeback thread paid later.
  `writeback_bytes` over the sweep fell from 79 MB to 8 MB, which is the same
  fact from the other side. The new number is what the write actually costs.
- `root formatted` did not move at first (1.6 -> 1.7 s), which pointed at
  `efs-mkfs` rather than the kernel. `zero_blocks` did a seek and a 4 KiB write
  per block, and formatting a 5 GB filesystem zeroes about 13000 blocks between
  the inode tables and the journal — at roughly 100 us per command that was the
  entire phase. Zeroing in 1 MiB chunks took it to **0.3 s** and the install to
  **2.8 s**.

The install is now 2.8 s from 4.3 s, and the remaining 2.2 s of it is the final
flush. That is the next thing to look at, and it is the same durable-write
throughput that bounds everything else.

## 3. The mmap fault path costs 121 us per page

A full map, fault in 4 MiB, unmap cycle is 124 ms — 1024 pages at 121 us each,
against 527 us for a `read` of the same 4 MiB. But the cost is **not** filling
pages from disk, which is what it looked like before the benchmark was fixed.

Two corrections got to that:

- `fsbench`'s mmap test timed only the memory sweep and left the `mmap` and
  `munmap` outside the operation, so it reported a rate computed over the whole
  loop while the latency column covered a fraction of it. The two disagreed by
  a factor of sixty. Both calls are inside the timed operation now.
- The test remaps the same file every pass, so after the first pass every page
  is already in the inode page cache. Whatever those 121 us are, no device is
  involved.

**Filling ahead was tried and reverted.** On a fault, fill a run of pages
through the existing `get_or_fill_bulk_async_sync` rather than one, so a
sequential walk pays one command per run. It moved the number by nothing
(26.3 -> 28.6 MiB/s, inside run-to-run noise), for the reason above: the pages
were already cached, so the fill-ahead correctly did nothing. It also broke the
mmap store test on the first attempt — a bulk fill publishes failure for its
whole range, so including the faulting page in that range let a bulk failure
kill the faulting thread. Narrowing the range to exclude the faulting page
fixed that, but an unproven change in the page fault handler is not worth
keeping.

What the measurement points at instead is **fault-around**: mapping several
PTEs per fault rather than one, so 1024 pages cost far fewer than 1024 faults.
That is a change to `FaultOutcome` and the VMA page-slot bookkeeping, which
currently carry exactly one page each. `munmap` of a large mapping is on the
same path and worth timing at the same time.

Anything attempting this should first measure a **cold** mmap — `fsbench write`,
reboot, `fsbench read` — so the fill cost and the fault cost can be told apart
rather than assumed.

## 4. Detached pages are a crash-consistency risk## 4. Detached pages are a crash-consistency risk, not just a slow path

A clean benchmark run reports `detached_fallbacks: +2626`. When a shard is
full, `read_page_for_write` gives up after `WRITE_PAGE_ATTEMPTS` and returns a
detached page, and `publish_write` then writes it straight to its home location
— ahead of the journal committing it. The comment on `read_page_for_write`
states the consequence: a replay after a crash overwrites newer data with the
journal's older copy.

Thousands per run means the escape hatch is firing routinely rather than
exceptionally, and the 8 MiB cache (8 shards x 256 pages) is too small for the
metadata working set. Size the cache to the working set, and treat a detached
write as something to count and alarm on rather than to absorb silently.

## 5. The allocating write path rewrites the inode per block

`write 512B` allocating runs at 3.8 MiB/s against 16.1 MiB/s overwriting the
same blocks. `ensure_block_for_logical` does a `read_inode`, an extent parse
and a `write_inode` for every 4 KiB block.

`ensure_blocks_for_logical_batch` already does the batched version, but it also
writes the inode itself, so it cannot simply be substituted: doing that
produced a segfault inside a `MAP_SHARED` mapping. Give it a mapping-only mode
whose caller owns the inode write.

## 6. Small, cheap

*(`sys_mmap`'s per-mapping log is done — it is `log_debug!` now.)*

- `scripts/fs-regression` still has to be run by hand after `make all`. Wire it
  into a make target alongside `fsbench`, so a durability regression and a
  throughput regression are both caught by the same command.
- The evict queue holds 256 entries and fills during an ordinary create/unlink
  burst, falling back to synchronous eviction. The log flood is fixed and the
  fallback is counted in `/proc/evict_stats`; the capacity and the drain rate
  behind it are not.

## Before chasing the per-command floor

4 KiB access is bounded by roughly 100 us per command on both read and write,
and coalescing cannot help a single page. Before attributing that to the driver,
do one release-profile run: the kernel builds dev-profile with debug assertions,
so the lock-order tracker runs on every `ranked_lock!`, and how much of the
100 us is tracker overhead rather than real work is currently unknown.
