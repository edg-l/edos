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

The reason is structural, not a slow submit path. **Almost every I/O path in the
kernel is submit-then-wait.** `block_read` / `block_write` / `block_write_fua`
in `fs/efs/mod.rs`, `read_frame` / `write_frame` / `write_frames` in
`fs/block_page_cache.rs`, and the equivalents in `fs/journal/replay.rs`,
`fs/fat32/`, `fs/gpt.rs` and `fs/mbr.rs` all call `submit_*` and immediately park on the
handle.

Three paths do not. `BlockPageCache::read_pages` → `submit_read_batch` coalesces
contiguous misses first, so a 1 MiB miss is two commands rather than 256. Since
2026-08-12 EFS `flush_pages_bulk` issues every run of a chunk before reaping any
of them, capped at 16 outstanding (below `OWNED_OPS_CAP`), with each staging
buffer held until its own handle completes. `fsbench write -n 32 /var` used to
report `ahci_stats.ncq_max_inflight +1` for a whole suite; it now reports +3 to
+9. And the journal committer queues a transaction's descriptor, data and revoke
blocks together through `RingWrites`, draining them at the barrier that already
had to precede the commit block. Block-page-cache writeback batches too, since
2026-08-12: `flush_dirty_once` collects dirty pages that pass its filters and
`write_batch` submits up to 8 of them before reaping any. Its cap is not the
device but `LOCK_RANK_DEPTH` — a batch holds one `page.write_lock` per
outstanding write. And since 2026-08-12 `read_via_extents` plans every run of a
bulk file read before issuing any: a range that spans several extents, or that
is longer than the 992 KiB one command carries, goes out as one queue of up to
16 commands (or 2 MiB of staging, whichever comes first) instead of one round
trip per run. The mount-time paths (`fs/journal/replay.rs` home blocks,
`fs/fat32/`, `fs/gpt.rs`, `fs/mbr.rs`) still do not.

That read change does not move `fsbench ra` on a contiguous file: its window is
128 pages, one run, and every async window takes the single-submit prefetch
path. What it changes is the fallback — a fragmented file, and any single read
larger than one command. `fsbench fragprep` builds the fragmented case, and
`fsbench ra` on it went from 5 async windows and 243 sync fallbacks to 248 async
and 0 declined once prefetch learned to plan multi-extent windows.

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
Writeback asks, and so does the journal committer; what is left is the read side
outside readahead and the mount-time paths.

Readahead has half of that path now (section 1b): a window is submitted before
the reader parks, so a command is outstanding while the reader waits. On a
contiguous file depth still never exceeds 1, because the reader joins that same
window's handle rather than issuing one of its own, and a 512 KiB window of one
extent is a single command; a window spanning several extents is now one command
per run. Reaching depth on the contiguous case means several windows in flight
at once, or splitting one.

Coalescing cannot help here — one page is one command — so this is
per-operation work, not another run-length fix.

### What a commit costs, measured: the flush barrier is now the biggest third

`/proc/journal_stats` counts commits, the ring blocks and device commands they
took, and the microseconds spent in each of the three device steps a commit
makes: the queued ring batch, the cache-flush barrier that orders it, and the
FUA commit block. `fsbench` samples it like the other counter files, and the
three `*_per_*` lines are boot-wide averages rather than totals, so a run's own
average comes from dividing the totals.

It also reports `sealed`, `pending` and `tracked`, the queue depths
`needs_checkpoint` answers from, summed over every mounted journal. `pending`
stuck at a non-zero value while `tracked` is 0 means transactions are committed
and fully checkpointed but never retired, which is what made `sync` run to its
round cap on every call before `advance_tail`'s retire bound was fixed.

A fresh-disk `fsbench write -n 32 /var`, read from a clean boot:

```
commits: 95          ring_blocks: 31392    data_blocks: 31200
commands: 312        checkpoints: 5        empty_commits: 0
ring_us: 115811      flush_us: 218290      commit_us: 107023
```

330 ring blocks per commit in 3.3 commands: the batch is being coalesced about
as well as the 248-entry PRDT allows, which is what the `RingWrites` change was
for. What it leaves is a commit costing 4.6 ms, of which the batch is 1.2 ms,
the commit block 1.1 ms, and **the flush barrier 2.3 ms — half the total, for
one command that carries no data.**

### Batching the sealed queue behind one barrier does not pay (measured, refuted)

The obvious reading of the numbers above is that two commits in a row pay two
barriers where one would order both, so `seal_and_commit` should prepare the
whole sealed queue, wait for it, issue one barrier, and then write every commit
block. That was built and measured: descriptor/data/revoke blocks of up to eight
transactions queued into one `RingWrites`, one `block_flush`, then the commit
blocks queued together with `WriteFlags::FUA` (safe, because replay stops at the
first transaction with no commit block, so a crash part-way through a batch
leaves a committed prefix — §14).

On the same `fsbench write -n 32 /var` from a fresh disk it reported **97
commits sharing 84 barriers**: 13 batches of two in the whole run, and nothing
larger. The sealed queue almost always holds exactly one transaction, because
the committer is woken by `kick_committer` the moment one is sealed and commits
it before another can be. There is nothing queued to coalesce.

So the barrier can only be shared by *delaying* a commit that is ready — sealing
on a timer, or letting a second `force_commit_and_wait` join a barrier already
in flight — and that trades fsync latency for barrier count, which is a policy
choice rather than a free win. Batching what happens to be queued is not.

Two things the attempt did establish, worth keeping if it is ever revisited:
queueing the FUA commit blocks together halved `commit_us` (107 ms → 57 ms
across a run), and timing must start *after* the ring-space reservation loop:
`checkpoint_and_advance` runs inside it and writes dirty pages home, so a
`t_ring` taken before it charges a checkpoint's whole flush to `ring_us` and
inflated it 116 ms → 599 ms in that measurement.

## 1b. Pipelined readahead: the instrument, and the baseline it reads

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

So the idea is not refuted, but nothing measured it. What survives is the
large-file case, above the whole-file threshold, where `page_cache_read_core`
(`fs/vfs.rs`) still extends only `window_size` pages past each request and
submits from within the read call.

**The instrument now exists**: `fsbench raprep /var`, reboot, `fsbench ra /var`
is a cold sequential pass over a 16 MiB file in 64 KiB calls — 32 windows of
`RA_MAX_PAGES`, well past the whole-file threshold. Beside throughput it reports
the three numbers that actually answer the question, documented in
`doc/fsbench.md`: how many calls stalled waiting on I/O nobody had started,
whether `ncq_inflight` was ever non-zero *between* calls, and how far
`ncq_max_inflight` rose across the pass. A change that leaves the inflight
samples at zero and the high-water mark where the boot left it has pipelined
nothing, whatever it did to the MiB/s.

**Baseline, 2026-08-12, cold boot, 4 vCPUs, 16 MiB in 256 calls of 64 KiB:**

| | |
|---|---|
| read path | 222 MiB/s, 72.1 ms summed (wall 75.8 ms) |
| per call | p50 174 us, p99 868 us, max 1.9 ms |
| stalls | 11 of 256 calls over 696 us |
| `ncq_inflight` between calls | non-zero on 2 of 256 samples, max 1 |
| `ncq_max_inflight` | 4 before, 4 after |

That is the trailing prefetch, measured rather than argued. Two readings matter.
The device was idle between calls in 254 of 256 samples and never held more than
one command, so the only I/O in flight at any moment was the one the reader was
blocked on — the window is refilled from inside the read call, so it cannot be
otherwise. And the stall count is 11, not the 32 that one stall per 512 KiB
window would give, because the window does ramp to `RA_MAX_PAGES` and a 64 KiB
call inside a filled window is a cache hit: `block_cache` took 618 hits and 1
miss across the pass.

So the target for a pipelined version is specific: the same 16 MiB with
`ncq_inflight` non-zero across most samples, `ncq_max_inflight` rising above the
boot's 4, and the 11 stalls going towards 0. Throughput is the last thing to
look at, not the first — 222 MiB/s is already far off the raw-device ceiling for
reasons section 1 owns.

**Two of those three criteria turned out to be untestable on this host**, for a
reason only visible once pipelining landed: the window completes inside the call
that issued it, so a sample taken between calls can never see it. Read the stall
count and p50; the reasoning is under "Pipelining landed" below.

### The three paths, and why the counter had to come first

Reading `page_cache_read_core` (`fs/vfs.rs`) against that baseline, a readahead
window past `end_page` can take three different paths, and **the instrument
cannot tell which one it took**:

1. `submit_prefetch_pages` returns `Ok(Some(..))` and `issue_prefetch_bulk`
   installs the pages — genuinely asynchronous, the reader does not wait.
2. It returns `Ok(None)` and the window falls back to
   `get_or_fill_bulk_async_sync` — a **synchronous** bulk fill, billed to the
   reader inside its own read call. EFS returns `None` for inline-data inodes
   and for a range that maps nothing at all; a fragmented range is queued as
   several runs rather than declined.
3. It returns `Err(..)` and takes the same synchronous fallback.

Path 2 produces exactly the signature the baseline recorded — the device idle
between calls and never more than one command outstanding — without any of it
being evidence about trailing versus pipelining, because on that path the
prefetch is not asynchronous at all. The two hypotheses are not distinguishable
from throughput, stalls or `ncq_inflight`.

A second number is unexplained and points the same way: p50 is 174 us per 64 KiB
call while only 11 of 256 calls are counted as stalled. In steady state the
window has ramped to `RA_MAX_PAGES` and the requested pages are 128 pages behind
the prefetch frontier, so the call should be a `memcpy` and finish in single-digit
microseconds. 174 us is a call waiting on a device, and the stall counter does not
see it — consistent with the wait happening inside the fallback bulk fill rather
than at the point the stall counter watches.

The counters that tell them apart were built for exactly this, and what they
read is below: path 1 dominates, 245 windows to 8. Path 2 is not what the
baseline was measuring, and the extent work is not what fixes this.

The fixture worry that came with the hypothesis is settled by the same reading.
`fsbench raprep` builds its 16 MiB file by appending, the pattern that can split
one contiguous extent into many tiny ones — but EFS declined only 8 of 253
windows, so `ensure_block_for_logical` is coalescing and the file is not badly
fragmented. The scaffolding is ruled out. Appending allocation has since been
batched and given a goal (see "Recently closed"), so the coalescing no longer
depends on the allocator happening to hand back neighbouring blocks.

### What the branch counter found: the prefetch reads and throws away 92 MiB

Built and read 2026-08-12, cold boot, same 16 MiB pass. The counters are
`/proc/readahead_stats`, reported by `fsbench ra`:

| | |
|---|---|
| windows async | 245 (30480 pages), **185 discarded (23680 pages)** |
| windows sync fallback | 8 declined (1024 pages), 0 failed |

So the path-2 hypothesis below is **refuted**: 245 of 253 windows do reach the
asynchronous path, and EFS declines only 8. The real defect is one layer down,
and the two numbers that give it away are in the async row itself.

`issue_prefetch_bulk` (`fs/page_fill.rs`) installs a `PageFillHandle` over the
whole window and **bails if any page in the range is already in flight** — it
narrows nothing. But `page_cache_read_core` submits the block I/O *before* it
tries the install, so a window that loses that check has already issued a real
AHCI read whose result nobody keeps: the buffer completes into a `Shared` Arc
that is dropped, the pages never reach the inode page cache, and the next call
finds the same range uncached and submits it again.

That is what 185 of 245 means. A 64 KiB call advances the reader 16 pages while
the window reaches 128 pages past it, so consecutive windows overlap by ~112
pages and almost every one collides with the previous window's still-installed
handle. 30480 pages of prefetch were submitted for a 4096-page file — 7.4x the
file — and 23680 of them, about 92 MiB, were read from the device and discarded.

This also explains the number section 1b could not: p50 168 us per call in a
window that should already be filled. The window is not filled, because the
fill that would have populated it was dropped, so nearly every call pays for
device I/O while the stall counter — which watches for a call far slower than
the median — sees nothing unusual, because *every* call pays it.

**So pipelining is not the fix, and neither is the extent work.** The fix is to
stop the two from colliding, in this order:

1. Do not submit I/O the install may discard: check `in_flight` for the window
   range *before* `submit_prefetch_pages`, and narrow the window to the pages
   that are not already covered (or skip it entirely when they all are). The
   check and the install must be under the same `in_flight` lock acquisition or
   the race just moves. That alone removes the 92 MiB and lets the first
   window's prefetch survive to serve the calls behind it.
2. Only then re-measure, and only then ask whether the prefetch still trails.

Rank note for whoever writes it: `in_flight` is `RANK_IN_FLIGHT`, and the
submit path takes driver locks, so the lock must be dropped before
`submit_prefetch_pages` — which is why the narrowed range has to be computed
first and the install re-checked after, not held across the submit.

### Step 1 landed: the 92 MiB is gone, and it cost 15% of the throughput

`page_fill::narrow_prefetch_window` now trims a window to the tail past its last
in-flight page before anything is submitted, and skips it outright when the whole
range is already being filled. `issue_prefetch_bulk` keeps its collision check as
the race backstop. Two counters were added beside the branch counters,
`skipped_*` and `trimmed_*`, so the trim is visible rather than inferred.

Same cold pass, 16 MiB in 256 calls of 64 KiB, 4 vCPUs, before and after:

| | before | after |
|---|---|---|
| read path | 222 MiB/s, 72.1 ms | **189 MiB/s, 84.7 ms** |
| per call | p50 174 us, p99 868 us, max 1.9 ms | **p50 307 us**, p99 1.2 ms, max 1.5 ms |
| stalls | 11 of 256 over 696 us | **2 of 256** over 1.2 ms |
| windows async | 245 (30480 pages), 185 discarded (23680) | 240 (4048 pages), **0 discarded** |
| windows overlapping | — | 13 skipped (784 pages), 240 trimmed (**26480 pages not re-read**) |
| `ncq_inflight` between calls | non-zero on 2 of 256 | non-zero on 0 of 256 |

The prefetch now reads the file **once**: 4048 async pages plus 576 declined
pages is 18 MiB for a 16 MiB file, against 110 MiB before. The stall count fell
from 11 to 2 because a window that survives really does serve the calls behind
it — `inflight_stats.joins` is 240, one per window, where the reader used to find
its pages uncached and refill them.

**And the pass got 15% slower.** The mechanism is not subtle, and it is the same
trailing prefetch section 1b named: the window is still submitted from inside a
`read`, so the call that triggers it waits for the *whole* window rather than for
its own 16 pages. Before, the surviving handle was the exception, so most calls
waited on a 16-page bulk fill; now most calls join a ~112-page one. Per call that
is 174 us → 307 us; in total it is 12 ms of extra summed wait to save 92 MiB of
device reads.

**Which way that trade goes depends on the host, and here the host is lying to
us.** `sata-disk.img` is a qcow2 file in the host's page cache, so the 92 MiB of
discarded reads were served from host RAM at nearly no cost while the longer
single wait was paid in full. On a real device, where 6x the read volume is 6x
the time, the same change reads as a straight win. Do not quote the 15% as a
verdict on the fix; quote it as the cost of the trailing prefetch.

So this is now the justification for pipelining that section 1b could not
produce. The 92 MiB was masking it: with the waste removed, the remaining cost is
exactly "the reader waits for a window it should not have had to wait for", and
firing the next window when the reader touches the last page of the current one
is what removes it. The target is unchanged — `ncq_inflight` non-zero across most
samples, `ncq_max_inflight` above the boot's 2, p50 back under 174 us.

### The 15% was not the trailing prefetch: the bulk fill re-read what it joined

The paragraph above named the wrong mechanism, and the counter that says so was
already on the report. `inflight_stats.retries` was 240 on a 256-call pass.

`get_or_fill_bulk_async_sync` (`fs/page_fill.rs`) aborts its install when any
page of the range is already in flight, parks on the conflicting handle, and
retries from the top of the loop — and the retry did not look at the page map.
So the reader joined the prefetch handle covering exactly its 16 pages, finalized
it (which publishes every page), woke, and then installed a fresh handle over
those now-cached pages and issued a device read for them. A second full read of
the file, once per call. The single-page `get_or_fill_async_sync` re-checks the
map both on entry and after its park; the bulk twin never did.

Narrowing is what exposed it: while the windows were being discarded there was no
surviving handle to collide with, so there was no join, no retry and no second
read. The fix is the missing re-check at the top of the `'outer` loop.

Same cold pass, all three states:

| | before narrowing | narrowed | + bulk re-check |
|---|---|---|---|
| read path | 222 MiB/s, 72.1 ms | 189 MiB/s, 84.7 ms | **260 MiB/s, 61.6 ms** |
| per call | p50 174 us, p99 868 us | p50 307 us, p99 1.2 ms | **p50 210 us, p99 509 us** |
| stalls | 11 of 256 over 696 us | 2 of 256 over 1.2 ms | **1 of 256 over 840 us** |
| device pages read | 30480 (110 MiB) | 4624 (18 MiB) | 4624 (18 MiB) |
| `inflight_stats.retries` | — | 240 | 240, none doing I/O |

The pass is now faster than it was with 92 MiB of waste in it, and it reads the
file once. **Pipelining was still unbuilt at this point, and justified** (it
landed next, in the section below): `ncq_inflight` is
non-zero on 0 of 256 samples and `ncq_max_inflight` did not move off the boot's
value, so nothing is ever outstanding except the one command the reader is
waiting on. p50 210 us against a memcpy's single-digit microseconds is what that
costs. The target for the pipelined version is unchanged.

### Pipelining landed: submit the window before filling the request

`page_cache_read_core` did the two halves of a read in the wrong order. For each
uncached run it filled the request portion first — which parks the reader on the
device — and only afterwards submitted the readahead window past `end_page`. So
the queue was empty for the whole of that park, and the window started only once
the reader no longer needed the overlap: the prefetch trailed the reader by a
full round trip rather than pulling ahead of it.

The loop is now three passes over `uncached_ranges`: submit every window, then
fill the request portions, then run whatever windows fell back to a synchronous
fill. The fallback is deferred past the request on purpose — that path is billed
to the reader, and the reader's own 16 pages must not queue behind 128 pages of
readahead.

Same cold pass, 16 MiB in 256 calls of 64 KiB, 4 vCPUs:

| | trailing | pipelined |
|---|---|---|
| read path | 260 MiB/s, 61.6 ms | **292 MiB/s, 54.9 ms** |
| per call | p50 210 us, p99 509 us, max — | **p50 203 us, p99 471 us, max 564 us** |
| stalls | 1 of 256 over 840 us | **0 of 256** |
| device pages read | 4624 (18 MiB) | 4608 (18 MiB) |
| windows async | 240 (4032 pages), 0 discarded | 240 (4032 pages), 0 discarded |

**`ncq_inflight` is still non-zero on 0 of 256 samples, and here that is the
success case rather than the failure it was.** The window is a single 64 KiB
command against a qcow2 the host holds in its page cache, and the reader's park
on its own pages is ~200 us — far longer than the window takes. So the prefetch
completes *inside* the call that issued it, and `fsbench` samples between calls,
after it is already done. `ncq_max_inflight` staying at 1 says the same thing
from the other side: the reader never issues a command of its own, it joins the
window's handle (240 joins), so there is nothing for a second command to overlap
with. **The inflight criterion in the baseline above is therefore not a usable
test on this host** — it can only be met by a device slow enough to still be
working when the call returns. Read the stall count and p50 instead.

What is left in the 203 us is not readahead placement: the file is read exactly
once, no window is discarded, and no call stalls. It is the per-command cost
section 1 owns, paid once per 64 KiB by the joiner that finalizes the window.

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

- **A fragmented file lost whole 4 KiB blocks.** A newly allocated block was
  zeroed through the journal, and that zeroed copy could reach the home block
  after the direct data write had already landed on it — 289 of one file's 4096
  blocks were nowhere in the image. The write path no longer stages a zeroed
  copy of a block it is about to overwrite whole (`554b515`,
  `doc/WORKING-NOTES.md` "Interleaved appends drop whole blocks on the write
  path").
- **Readahead declined any window that spanned more than one extent.** A
  prefetch window is now a set of contiguous runs, so a fragmented file goes
  from 5 asynchronous windows and 243 synchronous fallbacks to 248 and 0
  (`4f69216`, `kernel/src/fs/readahead.rs`).
- **An appending file asked for one block at a time.** `alloc_blocks` serves a
  whole batch, so a run of appends becomes one extent rather than one per block
  (`22b5783`, in-guest evidence at `67fa350`).
- **A file's next block is sought where its last extent ended.** `alloc_blocks`
  takes a goal from `ExtentMap::goal_for`, tries it exactly before any scan, and
  falls back to first fit inside the goal's own group, so batch N+1 continues
  batch N instead of restarting at group 0 (`ae0424a`). Measured on a fresh
  disk with `fsbench raprep /var` → reboot → `fsbench ra /var`: the 16 MiB
  appended file reads as **4 reads planned 4 runs, queued in 4 submits**, i.e.
  `runs / reads` = 1.00, so an appended file is laid out contiguously and a
  per-inode reservation window buys nothing on top.
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

Six experiments cost a build-and-boot each and are recorded so they are not
repeated. All of them came from reasoning that sounded right and was refuted by
measurement.

- **Batching the sealed transaction queue behind one flush barrier.** 97 commits
  shared 84 barriers, because the sealed queue almost never holds more than one
  transaction; see section 1 for the mechanism and for the two things the attempt
  did establish.

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
