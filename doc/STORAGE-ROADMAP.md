# Storage roadmap

What is left after the 2026-08-09 fsbench round, ordered by what the numbers
say rather than by how interesting it is. Every item names the evidence that
motivates it; re-measure with `fsbench` before and after.

The pattern behind the first item has now paid off three times: **issuing a
command costs far more than the sectors it carries.** Coalescing contiguous
blocks into one command took raw device reads from 37 to 886 MiB/s, `sync()`
after a write phase from 28.7 s to 21 us, and `mmap store + msync` from 8.8 to
1059 MiB/s. Anywhere a loop still does one 4 KiB command at a time is worth the
same treatment.

## 1. Find where the journal commit actually spends its time

A process's first `fsync` after a write phase that buffered hundreds of
megabytes costs 6-12 s, and `sys_fsync`'s own log attributes almost all of it
to `force_commit_and_wait`. Where it goes inside `seal_and_commit` is not yet
known. Measure before changing anything.

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

## 2. Fault-around for file-backed mappings

`mmap load 4MiB` faults in at 33 MiB/s against 1714 MiB/s for `read` of the
same bytes. Roughly 50x, and the mechanism is one fault and one fill per 4 KiB
page. Mapping the neighbouring pages that are already in the page cache on each
fault (Linux maps 16) should close most of it.

## 3. Detached pages are a crash-consistency risk, not just a slow path

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

## 4. The allocating write path rewrites the inode per block

`write 512B` allocating runs at 3.8 MiB/s against 16.1 MiB/s overwriting the
same blocks. `ensure_block_for_logical` does a `read_inode`, an extent parse
and a `write_inode` for every 4 KiB block.

`ensure_blocks_for_logical_batch` already does the batched version, but it also
writes the inode itself, so it cannot simply be substituted: doing that
produced a segfault inside a `MAP_SHARED` mapping. Give it a mapping-only mode
whose caller owns the inode write.

## 5. Small, cheap

- `sys_mmap` logs a line per file-backed mapping and floods `run_log.txt`
  during any run that remaps in a loop. Drop it to `log_debug!`.
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
