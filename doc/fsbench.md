# fsbench

`programs/fsbench` measures filesystem throughput and latency across the idioms
a program can actually choose between, at three depths:

| Depth | Command | What it bounds |
|---|---|---|
| memory filesystem | `fsbench /tmp` | the syscall and copy path; no disk involved |
| block device | `fsbench raw /dev/sda` | the block page cache and the AHCI driver |
| on-disk filesystem | `fsbench /var` | what is left after EFS |

Reading them together is the point: the gap between `/tmp` and `raw` is the
storage stack, and the gap between `raw` and `/var` is EFS.

## Running it

```
fsbench [MODE] [PATH] [OPTIONS]

Modes:
  all     write, read and metadata in one boot (default)
  write   write and metadata only; leaves its files for a later read run
  read    read only, against files a previous `write` run left behind
  raw     sequential reads straight from a block device, no filesystem
  clean   remove every file the suite creates

Options:
  -t MS   per-test time budget in ms (default 700)
  -m MIB  per-test byte cap in MiB (default 256)
  -q      quick: 200 ms budget
  -k      keep the files a run creates
  -l      mirror the report to /dev/klog, which the host reads off run_log.txt
  --no-verify
```

Each test runs until a time budget or a byte cap, whichever comes first, so a
path running at 1 MiB/s and one running at 1 GiB/s both take about the same
wall-clock second and the suite finishes either way.

`-l` is how a headless run gets read: the guest terminal holds far fewer lines
than a full report prints, and `/dev/klog` is teed to `run_log.txt`.

### Cold reads need two boots

`fsbench all` reads files it wrote moments earlier, so its read numbers are
page-cache hits. For disk reads use `fsbench write /var`, reboot, then
`fsbench read /var`. Raw device reads need no such care: the swept span is far
larger than the 8 MiB block page cache, so it evicts itself as it goes.

### What it checks besides speed

Every write is a position- and size-dependent pattern, re-read and compared
after the write phase. A block written at the wrong offset, a stale block and a
block of zeros all fail; a constant fill would catch none of them. The report
names the first bad offset, how many bytes are wrong, and whether the tail is
zeros.

The run also prints the delta of every counter in `/proc/block_cache`,
`/proc/ahci_stats`, `/proc/inflight_stats` and `/proc/evict_stats`. That is what
turns a number into a diagnosis: a "cold" read reporting no cache misses never
touched the disk.

## Findings, 2026-08-09

Host reference first, so the guest numbers have a ceiling to be compared
against. `qemu-img bench` against the same qcow2, host page cache warm:

| Request | Host backend |
|---|---|
| 1 MiB | ~10 GB/s |
| 64 KiB | ~4.3 GB/s |
| 4 KiB | ~482 MB/s, ~123k IOPS |

The host storage backend is nowhere near being the limit. Better still, the
swept region of the image is unallocated, so QEMU answers those reads from
nothing at all: what the guest measures is the guest's own overhead plus QEMU's
AHCI device model, with host storage removed from the question entirely.

### 1. Command count, not queue depth, bounded the raw device

*(Fixed. The numbers below are what led to it; see the end of this section for
the result.)*

The same `read_bytes` code, the same block page cache, the same eviction
pressure, against two different devices:

| Request | `/dev/ram0` (no AHCI) | `/dev/sda` (AHCI) |
|---|---|---|
| 512 B | 21 MiB/s | 13 MiB/s |
| 4 KiB | 143 MiB/s | 31 MiB/s |
| 64 KiB | 892 MiB/s | 39 MiB/s |
| 1 MiB | 1130 MiB/s | 37 MiB/s |

Both runs genuinely miss the cache (160k and 21k misses respectively, with
evictions to match), so this is not a warm-versus-cold artefact. The ramdisk
scales with request size the way it should; `/dev/sda` is flat from 64 KiB
upwards at roughly 100 us per 4 KiB page no matter how much is asked for.

Everything above the driver is therefore exonerated: the page cache, the
byte-range API and the copy path all deliver over a gigabyte per second when
the device underneath is memory.

`/proc/ahci_stats` now reports `ncq_max_inflight`, and that is what identifies
the remaining cost. A sweep that submits 64 commands per batch reaches a
high-water mark of **9** outstanding commands against a queue negotiated at
depth 32. The commands do overlap, so the driver is not fully serial, but the
queue never fills: submission cannot outrun completion. The ~100 us per page is
being spent getting a command *issued*, not waiting for QEMU to answer it.

Batching `read_bytes` through `read_pages`, so all misses are submitted before
any is waited on, moved the number by nothing: 37.8 -> 37.9 MiB/s. Depth was
never the problem.

What was: `read_pages` issued **one command per page**. EFS reads the same
drive at over 1.7 GiB/s because `read_via_extents` asks for up to 992 KiB at a
time. Coalescing consecutive misses into one command each closes almost all of
the gap:

| Request | Per page | Coalesced | Ramdisk (no AHCI) |
|---|---|---|---|
| 4 KiB | 31 MiB/s | 32 MiB/s | 143 MiB/s |
| 64 KiB | 39 MiB/s | **364 MiB/s** | 892 MiB/s |
| 1 MiB | 37 MiB/s | **886 MiB/s** | 1130 MiB/s |

`ncq_max_inflight` drops to 1 afterwards, which is the confirmation: a 1 MiB
read is now a single command, so there is nothing left to queue.

4 KiB is unchanged because a 4 KiB read is one page and has nothing to
coalesce with. Each command costs roughly 100 us of submission regardless of
size, so single-page access is bounded by that; readahead, not batching, is
what would help there.

### 2. Every write number except the fsync rows measures the page cache

EFS on `/var`, 700 ms per test:

```
WRITE                                MiB/s     ops/s      p50      p99       max
write 512B  seq, allocating            3.8      7757    103us    203us     918us
write 4KiB  seq, allocating           27.7      7080    119us    210us     3.3ms
write 64KiB seq, allocating            162      2600    250us    938us     2.5ms
write 1MiB  seq, allocating            278       278    2.9ms    4.0ms     5.8ms
write 512B  via 64KiB BufWriter       17.5     35757      6us     37us     3.6ms
fs::write 1MiB whole file, 1 call      369       369    2.0ms    7.1ms    23.9ms
pwrite 64KiB seq, positional           147      2349    273us    1.1ms     5.7ms
overwrite 512B  seq, in place         16.9     34617      9us     30us     2.1ms
overwrite 4KiB  seq, in place          112     28771     12us     39us     4.0ms
overwrite 64KiB seq, in place          316      5055     92us    546us     2.6ms
overwrite 1MiB  seq, in place          403       403    1.5ms    3.5ms     5.1ms
pwrite 4KiB random offsets             110     28128     12us     41us     5.1ms
write 1MiB + fsync each                9.5       9.5   97.0ms  147.6ms   147.6ms
```

A `write(2)` returns as soon as the bytes are in the per-inode page cache; the
device does not see them until writeback runs, long after the test that
produced them stopped timing. Only the `+ fsync each` rows measure the disk.
278 MiB/s buffered against 9.5 MiB/s durable is that gap.

Two idiom results worth keeping:

- A 64 KiB `BufWriter` turns 512-byte writes from 3.8 MiB/s into 17.5 MiB/s.
  Four and a half times, for free, in userspace.
- Allocating costs real time: `write 4KiB` allocating is 27.7 MiB/s against
  112 MiB/s overwriting blocks the file already owns.

Reads scale the way writes do not, because `read_via_extents` already issues
bulk commands up to 992 KiB: 21 MiB/s at 512 B, 151 at 4 KiB, 1047 at 64 KiB,
1739 at 1 MiB (page-cache warm).

### 3. `mmap` is the slowest way to touch a file

`mmap load 4MiB` faults in at 25 MiB/s against 1739 MiB/s for a `read` of the
same bytes, roughly 70x. `sys_mmap` also logs a line per file-backed mapping,
which floods `run_log.txt` during any run that remaps in a loop.

## Bugs this turned up

Fixed:

- **`File::sync_all()` and `sync_data()` were silent no-ops** in the std fork
  (`library/std/src/sys/fs/edos.rs` returned `Ok(())` without calling
  anything), while `edos_rt::fd::fsync` existed and worked. Every std program
  that called `sync_all` believed its data was durable when nothing had been
  flushed. Both now reach `SYS_FSYNC`.
- **`Journal::force_commit_and_wait` could never reach its target.** It waited
  for `committed_seq >= state.active.seq`, but `seal_active` deliberately
  leaves an *empty* active transaction in place, so that sequence is never
  sealed and never committed. With an empty active transaction and sealed ones
  pending, `committed_seq` tops out at `active.seq - 1` and the call ran its
  full 30 s deadline before returning `IoError`. `sync()` hit this and took
  exactly 30.00 s. It now targets the last sealed transaction when the active
  one is empty.
- **A create/unlink burst floods the log.** The evict queue is 256 deep and
  fills in well under a second, and the synchronous fallback logged a line per
  inode. It is now counted in `/proc/evict_stats` as `sync_fallback_count` and
  logged once per 512.
- **Full-page writes to a block device read the page first.** Whether a page
  needs its old contents and whether the cache has room for it are separate
  questions, and `write_bytes` conflated them by always going through
  `read_page_for_write`. A page the write covers completely is no longer read.

Fixed after the first round:

- **A timed waiter that wakes early is never woken again.** `wake_one` and
  `wake_all` pop entries off the queue, so being woken unregisters the waiter,
  and the timed arm of `WaitQueue::wait_internal` looped and slept again
  without re-enrolling. Any wake arriving while the predicate was still false
  consumed the registration, every later wake missed the thread, and it slept
  out its whole deadline before noticing the condition had become true.
  `force_commit_and_wait` hit this: it waits for a target sequence while the
  committer wakes `commit_wq` for each transaction it finishes, so an
  intermediate wake landed first and the caller sat on its 30 s deadline and
  then returned success — silently, with no timeout logged, because it had not
  actually timed out. That is where the "exactly 30.000 s" came from.
- **EFS wrote file data one 4 KiB page per command.** `flush_pages_bulk`
  batched the block mapping and then issued one command per page; the same
  defect as the block page cache's read path, in the filesystem. Contiguous
  blocks now go out as one command, and `write_via_extents` no longer reads a
  block it is going to overwrite completely.

A later round coalesced the journal ring writes too — `seal_and_commit` wrote
each enrolled block with its own command — which took the first `fsync` from
12.6 s to 1.6 s and the whole suite from 20.6 s to 10.1 s.

Result on the same run:

| | Before | After |
|---|---|---|
| `sync()` after the write phase | 28.7 s | **21 us** |
| `write 1MiB + fsync each` | 4.6 MiB/s | **17.8 MiB/s** |
| `mmap store 4MiB + msync` | 8.8 MiB/s | **1059 MiB/s** |
| whole suite | 86 s | **19.9 s** |

The first `fsync` of a process still costs seconds (12 s here) after a write
phase that buffered hundreds of megabytes, but that is now the journal commit
doing real work rather than a wait sitting on a deadline, and it is attributed
as such in the log.

Rejected hypotheses, recorded so they are not retried:

- `enter_legacy_mode` starving behind `enter_ncq_mode`. Giving legacy commands
  writer preference made things worse — `write 1MiB + fsync` went from 97 ms to
  30 s — and was reverted.
- Routing `write_via_extents` through `ensure_blocks_for_logical_batch`. That
  helper writes the inode itself, and the result was a segfault inside a
  `MAP_SHARED` mapping; reverted, keeping only the full-block read skip.

Also fixed in that round: `sync` committed the journal and *then* flushed, but
a flush pass enrols the metadata that maps the file data it writes, and
writeback will not check point a block whose transaction has not committed. So
`sync` returned with the extents for its own writes still in memory. Two rounds
of commit-then-flush now; verified by `scripts/fs-regression` reading back cold
after a reboot.

Reference points: `edos-install` onto a blank 5 GB disk is 3.0 s (1.7 s
formatting the root filesystem, 1.0 s in the final flush, everything else under
0.2 s), and the installed disk reaches `edos-init` 1.44 s into the kernel and
comes up to a full desktop with no ISO attached.

FAT32 is not a limit anywhere measured: its I/O is already issued per cluster
rather than per sector, and it only carries the ESP, a few megabytes the
installer writes in 0.0 s. It does not coalesce contiguous clusters, so it
would benefit from the same treatment if it ever carries real load.

Still open, as performance rather than correctness:

- **4 KiB access is bounded by per-command cost**, roughly 100 us, on both
  read and write. Coalescing cannot help a single page; readahead and write
  clustering are what would.
- **`mmap` fault-in reads at 33 MiB/s** against 1714 MiB/s for `read` of the
  same bytes, and `sys_mmap` logs a line per file-backed mapping.
