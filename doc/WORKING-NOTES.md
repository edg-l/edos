# Working notes, sessions of 2026-08-08 to 2026-08-12

State of the tree, what changed, and what is still open. Written for whoever
picks this up next, which will usually be an agent with no memory of the
session.

---

## A batch of same-class guards is bounded by the rank stack, not by the device

Block-page-cache writeback (`flush_dirty_once`) wrote one dirty page per device
round trip. It now collects the pages that pass its filters and hands them to
`write_batch`, which submits them together and reaps afterwards.

Two things the lock discipline forces, both of which cost a boot to learn:

- **The batch cap is `LOCK_RANK_DEPTH / 2`, not a device queue depth.** Each
  outstanding write holds that page's `write_lock` until its DMA is done, and
  the lock-order tracker records at most `LOCK_RANK_DEPTH` (16) live guards per
  thread — a 16-deep batch fills the stack exactly and the next `ranked_lock!`
  panics with `lock_order stack overflow`.
- **Per-page bookkeeping cannot interleave with the reaping.** The first
  attempt dropped each page's guard right after its own `wait()` and then took
  `BPC.journals` (120) to report the checkpoint. That panics on the first boot:
  the *other* guards of the batch are still live, so the rank stack still has a
  140 entry. The bookkeeping runs in a second pass, after the last guard of the
  batch is gone.

`page.write_lock` is one lock class, so a batch holding several is a same-class
multi-acquire: `write_batch` sorts by `(device_id, page_block_idx)` and takes
them through `ranked_lock_same!` in that order.

Verified in the guest: desktop boots with no panic, `fsbench write -n 32 /var`
completes in 1.3 s reporting `ncq_max_inflight +7`, and `iotest /var` is 20/20.
The depth figure is not attributable to this change alone — EFS bulk flush and
the journal committer also queue — so treat it as "no regression, batching
still reached", not as a measurement of writeback depth on its own.

---

## Writeback queues now, so the drive is no longer asked one command at a time

`doc/STORAGE-ROADMAP.md` section 1 says every I/O path but `read_pages` is
submit-then-wait, and that `fsbench` reported `ahci_stats.ncq_max_inflight +1`
for a whole suite: the system never had two commands outstanding, so a 4 KiB
access cost a full dependent round trip.

`EfsDriver::flush_pages_bulk` (`fs/efs/mod.rs`) already coalesced a chunk's
pages into runs of physically contiguous blocks, but it called `block_write`
per run, which submits and parks. It now issues every run through
`submit_block_write` and reaps the handles afterwards, so the runs of one chunk
are queued together.

Measured in the guest, `fsbench write -n 32 /var` on a freshly formatted disk:
`ncq_max_inflight` +1 before, **+3 and +9 across two runs** after, with
`verify: all patterns match` on both. The per-row throughput is inside the
noise the roadmap warns about for the `/var` suite, which is expected — one
chunk of a sequential file is a couple of long runs, so depth is what moved,
not the bandwidth of a run.

Three things the shape of this had to respect:

- **A staging buffer is the DMA source on the direct path**, so it is held in
  the in-flight record and only dropped once its own handle completes. Returning
  early from a failed run without draining the rest would free buffers the
  drive is still reading.
- **Depth is capped at 16, below `OWNED_OPS_CAP` (32)**. Past that,
  `install_ncq_op`'s `owned_ops_push` fails and the command silently loses its
  cancellation hookup. `allocate_slot_blocking` parks when the hardware slots
  are gone, so the cap is about the registry, not the drive.
- **The block-cache invalidation stays after the wait**, and now happens on
  failure too: a command that reported an error may still have landed in part,
  so a cached page for that range cannot be trusted either way.

Still submit-then-wait, and the next place to take this: `block_read` /
`block_write` / `block_write_fua` themselves, `read_frame` / `write_frame` /
`write_frames` in `fs/block_page_cache.rs`, and the FAT32, GPT and MBR paths,
plus `replay.rs`, which writes home blocks one command at a time on the mount
path.

---

## The journal committer queues a transaction instead of writing it in three trips

`seal_and_commit` (`fs/journal/mod.rs`) wrote the descriptor block, then the
payload, then the revoke block, parking on each. The format does not ask for
that ordering: what it requires is that all three are on the platter *before*
the commit block, which the `block_flush` barrier and the FUA commit already
give. So the three are now queued through a `RingWrites` helper and drained at
that same barrier — one wait for the batch instead of three dependent round
trips per commit. Depth is capped at 16 for the same `OWNED_OPS_CAP` reason as
writeback.

Two shapes this had to respect, both the same class of bug as the writeback
work:

- **The revoke block's buffer is the DMA source**, so it is bound outside the
  `if` that builds it. Left inside the branch it would drop while its command
  was still outstanding.
- **`RingWrites::drain` waits for every command even after one has failed**, and
  only then reports. Returning early on the first error would free the payload
  buffer under the drive.

What was verified: `scripts/fs-regression` passes across a reboot (the durability
gate that matters for a journal change), and `fsbench write -n 32 /var` on a
freshly formatted disk reports `verify: all patterns match`, `ncq_max_inflight
+6`, `write 4KiB + fsync each` 52.0 ops/s at 2.6 ms p50.

`ncq_max_inflight` could not settle whether commit latency fell — it is a global
maximum and writeback alone drives it to +3..+9, so it cannot attribute depth to
the journal. `/proc/journal_stats` was added for that, and it says a commit costs
4.6 ms: ring batch 1.2 ms, flush barrier 2.3 ms, FUA commit block 1.1 ms, with
330 ring blocks per commit in 3.3 commands. The batching worked; the barrier is
now the largest third.

### Sharing that barrier between commits: measured and refuted

Coalescing whole transactions behind one barrier looked like the obvious next
step and does not pay: the sealed queue almost never holds more than one
transaction, because `kick_committer` wakes the committer as soon as one is
sealed. A build that prepared up to eight of them behind a single `block_flush`
reported **97 commits sharing 84 barriers** on `fsbench write -n 32 /var`. Full
numbers and the two useful findings that fell out of it (FUA commit blocks are
worth queueing; `t_ring` must start after the reservation loop, or a checkpoint's
flush is charged to `ring_us`) are in `doc/STORAGE-ROADMAP.md` section 1. The
barrier can only be shared by delaying a commit that is ready, which trades
fsync latency for barrier count.

---

## FIXED: one `ioctl` wedged a CPU, because a match scrutinee held its guard

`syscallfuzz` found this on its first run. It was userspace-reachable, it was
deterministic, and it took the whole machine down:

```
syscallfuzz -n 8 -v -u 0 -o ioctl
  ioctl  fxpnx
    case  0 [ffffffffffffff9c, 80000000, 436a41, 1001, ffffffff] ->
```

That is `ioctl(fd = -100, request = 0x80000000, arg = <a valid page + 1>,
arg_len = 4097, flags = 0xffffffff)`, and it never returns. Reproduced twice,
same case index both times, since the generator is seeded `seed ^ nr`.

What it does to the system, from `run_log.txt` of the first run:

```
[21.579] <cpu-0:/bin/syscallfuzz:u:27> Unmap partial error (kernel-managed VMAs in range)
[21.793] <cpu-2:/bin/edos-wm:u:25> tlb_shootdown: re-sending IPI to CPUs 0x1
[22.154] KERNEL PANIC: tlb_shootdown: CPUs 0x1 never acknowledged a flush of
         50 page(s) at VirtAddr(0x283f000) across 3 attempts
```

CPU 0 stops taking interrupts entirely, so the next unrelated `munmap` anywhere
in the system panics at `memory/tlb.rs:133`. The watchdog is doing its job; the
bug is whatever holds CPU 0.

The mechanism was one line, and it is a Rust rule rather than anything about
ioctl:

```rust
let descriptor = match info.lock().fd_table.lock().get_fd(fd).cloned() {
    Some(desc) => desc,
    None => {
        info.lock().errno = Errno::EBADF;   // <-- deadlock
        return -1;
    }
};
```

**Temporaries created in a `match` scrutinee live until the end of the whole
`match`**, so both guards are still held while an arm runs. `info` is an
`Arc<IrqSpinlock<UserThreadInfo>>`, so the `None` arm re-locks a spin lock this
CPU already holds, with interrupts disabled, and never leaves. The fd only has
to be one that is not open — `-100` was incidental. Any process could take the
machine down with `ioctl(-1, ...)`.

That also explains the shape of the panic: an `IrqSpinlock` spin never re-enables
interrupts, so CPU 0 stopped acknowledging IPIs, and the next unrelated `munmap`
on any other CPU tripped the TLB shootdown watchdog. The watchdog was the
messenger.

The fix binds the lookup to a `let` first, which drops both guards at the end of
that statement. `if let` is not affected: edition 2024 (which this kernel uses)
drops `if let` scrutinee temporaries before the body, and only `match` still
extends them.

Ruled out on the way, and worth not re-checking:

- **`FdTable::get_fd` has no special case for `-100`/`AT_FDCWD`**
  (`thread/fd.rs:65` is a plain map lookup), which is exactly why the deadlock
  is in the failure arm rather than in any device path.
- **Not the two `interrupts::enable()` calls in the `FsFile` branch**
  (`syscalls/ioctl/mod.rs`). They are redundant — `syscall_handler` already
  enables interrupts at `syscalls/mod.rs:1813` — but unreachable for a bad fd.

Left standing, and not a deadlock today: every other fd-table syscall
(`net.rs`, `memory.rs:326`, `mod.rs:1721`, `fs.rs:120`) clones the fd-table
`Arc` first and then sets `errno` from a match arm, so it holds `fd_table` while
taking `info`. That is the opposite order from the statement-level
`info.lock().fd_table.lock()` uses in `io.rs`, `fs.rs:611` and `thread.rs:1180`.
Nothing co-holds them the other way now that ioctl is fixed, but two threads
sharing an fd table is what would make it matter.

---

## FIXED: a user pointer was never bounds-checked, only fault-fixed up

`syscallfuzz -n 4 -u 0` panicked the kernel a second way: a General Protection
Fault in ring 0 inside `do_user_copy` (`util/uaccess.rs`), reached from
`sys_pipe`. The pointer was `0x0000_8000_0000_0000`, one of the fuzzer's poison
values.

The whole of `try_copy_from_user`/`try_copy_to_user`'s validation was a null
check. Everything else was left to the fault fixup, and the fixup is only wired
into the page fault handler, so two classes of address walked straight through:

- **Non-canonical** (`0x0000_8000_0000_0000`). Dereferencing one raises #GP, not
  #PF. `general_protection_fault_handler` had no `fault_resume` check, so the
  ring-0 arm panicked the machine on a pointer any program can pass.
- **The kernel half** (`0xffff_ffff_8000_0000`). That address is canonical and
  mapped, so the copy *succeeded*: `read(fd, kernel_addr, n)` overwrote kernel
  memory and `write(fd, kernel_addr, n)` handed kernel memory to userspace. No
  fault, no error, no trace of it.

Fixed at the source with an `access_ok(addr, len)` in `util/uaccess.rs` that
requires `addr + len <= USER_VA_END` with a checked add, applied to the user
side of both copy directions — `src` for `from_user`, `dst` for `to_user`. Every
other entry point (`try_read_user`, `try_write_user`,
`try_copy_string_from_user`) funnels through those two, and every call site in
the tree passes a pointer that came from a syscall argument, so there is no
in-kernel caller that legitimately needs the kernel half.

The #GP handler got the page fault handler's fixup as well. With `access_ok` in
place nothing should reach it, which is the point: a uaccess copy that faults
for a reason the checks did not anticipate now reports failure instead of
taking the machine down.

Left standing, deliberately: every exception handler in `interrupts/idt.rs`
sends `end_of_interrupt()` on entry, which is wrong for a fault (no interrupt is
in service) and would clear an unrelated ISR bit if a fault ever landed inside
an interrupt handler. Not reachable from the uaccess path, which never runs in
interrupt context, so it was left alone rather than churned.

---

## FIXED: `shm_create` reserved for a size before checking it was possible

The same fuzz run then panicked in the allocator:

```
failed to map heap expansion: FrameAllocationFailed   (allocator.rs:334)
  <- RawVecInner::try_allocate_in
  <- sys_shm_create (syscalls/shm.rs:44)
```

`SharedMemory::new` (`memory/shared.rs`) turned the caller's `size` into a frame
count and then `Vec::with_capacity(frame_count)`. The batched allocation loop
below it was careful — it releases the frame-allocator lock every 64 frames so
interrupts are not starved — but the reservation happens first, so a size that
no machine could satisfy grew the kernel heap until the frame allocator had
nothing left to expand it with. That path panics; it does not return a null.

Two fixes at the source: `size + 0xFFF` is a `checked_add` now (a size near
`usize::MAX` wrapped to an aligned size of 0 and a frame count of 0, so the
worst case was a zero-frame region reported as a success), and the frame count
is compared against `frame_allocator().stats().free_frames` before anything is
reserved. Over budget is `AllocationFailed`, which `sys_shm_create` already maps
to `ENOMEM`.

`stats()` walks the whole bitmap, which is ~128 KiB of `count_ones` on a 4 GiB
guest. `shm_create` is not a hot path — the compositor calls it per surface —
so the honest bound was preferred over a cheaper comparison against
`total_frames`.

---

## Readahead now submits before it fills, and "device idle" stopped meaning idle

`page_cache_read_core` (`fs/vfs.rs`) filled the reader's own pages first and
submitted the readahead window afterwards, so the queue was empty for the whole
of the reader's park and the window started only once the overlap was no longer
wanted. Submitting every window first, then filling the request portions, then
running the windows that fell back to a synchronous fill, took the cold 16 MiB
`fsbench ra` pass from 260 to 292 MiB/s and its stall count from 1 to 0. The
sync fallback is deliberately last: it is billed to the reader, and the reader's
16 pages must not queue behind 128 pages of readahead.

**The trap is in the instrument, not the code.** `fsbench ra` reports whether
`ncq_inflight` was non-zero *between* calls, and `doc/STORAGE-ROADMAP.md`
section 1b originally set "non-zero across most samples" as the target for a
pipelined version. That target cannot be met on this host and its absence is not
evidence of anything: the window is one 64 KiB command against a qcow2 the host
holds in RAM, and the reader's park is ~200 us, so the prefetch completes inside
the call that issued it and the between-calls sample always reads zero.
`ncq_max_inflight` stuck at 1 says it from the other side — the reader joins the
window's handle rather than issuing a command, so there is never a second one to
overlap. Judge this path by the stall count, p50, and the discarded/trimmed page
counters instead.

---

## Readahead submitted I/O it was about to refuse, and the host hid the cost

`page_cache_read_core` (`fs/vfs.rs`) built its readahead window from the inode
page map alone. A page an earlier window is still filling is in neither the map
nor the reader's way, so it looked uncached and went into the new window — and
since a 64 KiB call advances the reader 16 pages while the window reaches 128
past it, consecutive windows overlapped by ~112 of 128 pages. The submit ran
first and `issue_prefetch_bulk` refused the colliding range second, so the AHCI
read completed into a buffer nobody kept: 185 of 245 windows on a 16 MiB pass,
23680 pages, ~92 MiB read from the device and thrown away, with the same range
re-submitted on the next call.

The fix is `page_fill::narrow_prefetch_window`: take `in_flight` once, trim the
window to the tail past its last busy page, skip it when nothing is left, and
only then submit. The lock cannot be held across the submit (`RANK_IN_FLIGHT`,
and submitting takes driver locks above it), so the install re-checks — that
check stays as the race backstop, and `async_dropped_windows` stays as its
alarm. `/proc/readahead_stats` gained `skipped_*` and `trimmed_*`.

**The trap worth keeping: this made the benchmark 15% slower, and that is not a
verdict on the fix.** Discarded is 0 and 26480 pages are no longer re-read, but
p50 per call went 174 us → 307 us and the pass 72.1 → 84.7 ms, because a window
that now survives is a window the triggering `read` waits for in full — ~112
pages instead of its own 16. `sata-disk.img` is a qcow2 file sitting in the
host's page cache, so the 92 MiB of waste was served from host RAM at almost no
cost while the longer single wait was paid in full. Any storage measurement here
that trades read *volume* for read *latency* will read backwards for the same
reason. The full before/after table is in `doc/STORAGE-ROADMAP.md` section 1b.

The 15% turned out not to be the trailing prefetch after all — see the next
section. It was a second defect the waste had been hiding.

---

## A bulk fill that joined an in-flight range then read it again anyway

`get_or_fill_bulk_async_sync` (`fs/page_fill.rs`) installs one handle over the
whole range and, if **any** page in it is already in flight, aborts the install,
parks on the conflicting handle and `continue`s the outer loop. The retry went
straight back to the install phase. Nothing re-read the page map — so a join that
had just finalized the prefetch and published every page in the range still went
on to install a fresh handle, allocate the frames again, and issue a device read
for pages that were sitting in the cache.

The single-page `get_or_fill_async_sync` has had that re-check since it was
written (both after the park and as an entry fast path); the bulk twin never got
one, and its doc comment argued the retry was safe — which it was, only not free.

It stayed invisible while readahead was discarding its windows: with no surviving
handle there was no conflict, no join, and no retry. Narrowing made the handles
survive, every 64 KiB call started colliding with the prefetch that had been
issued for exactly its range, and the pass paid a second full read of the file.
That, not "the reader waits for the whole window", is where iteration 16's 15%
went. `inflight_stats.retries` is the tell: 240 retries on a 256-call pass, one
per join.

Fix: re-check the page map at the top of the `'outer` loop and return when the
whole range is present. Cold 16 MiB pass, `fsbench ra /var`:

| | before narrowing | narrowed | + this fix |
|---|---|---|---|
| read path | 222 MiB/s, 72.1 ms | 189 MiB/s, 84.7 ms | **260 MiB/s, 61.6 ms** |
| per call p50 | 174 us | 307 us | **210 us** |
| stalls | 11 of 256 | 2 of 256 | **1 of 256** |
| device pages read | 30480 (110 MiB) | 4624 (18 MiB) | 4624 (18 MiB) |

So the file is still read exactly once and the pass is now faster than it ever
was with the waste in it. `ncq_inflight` is still non-zero on 0 of 256 samples:
the prefetch is not pipelined, and that item is still open.

---

## `make test` used to pass without running

If a guest was already up — `make run-headless`, or anything else holding
`sata-disk.img` — `make test` exited **0 having run no tests at all**. The
suite reports through `isa-debug-exit`, which the host reads as
`(code << 1) | 1`, so a pass is exit 1; qemu's own startup failures are also
exit 1, and the `Failed to get "write" lock` on the disk is one of those. The
exit code alone cannot tell the two apart, so the only visible sign was that a
7-second gate came back in a fraction of a second.

The verdict now comes from the serial log: exit 1 must also carry
`TESTS PASSED` in `run_log.txt`, and the test targets `rm -f run_log.txt`
first so a previous run's verdict cannot stand in for this one. Both branches
were exercised — a normal run still passes, and a run with a guest holding the
disk now fails with `qemu exited before the suite reported a verdict`.

Worth generalising: **a gate that cannot fail is not a gate.** Any check whose
success is inferred from an exit code shared with the harness's own failures
needs a positive signal from inside the guest.

## The ISO `make test` leaves behind boots to a black screen

Directly after a test target, `edos-x86_64.iso` is a `--features sched-test`
build, and that kernel runs the suite and stops rather than continuing to the
desktop. `make run-headless` and `make storage-check` both take the ISO as
already built, so they boot it and the guest looks hung: the serial log ends at
`ALL 51 TESTS PASSED` and every screenshot is black. `make all` restores the
normal ISO in about three seconds, because cargo still has the non-feature
artifact and only the image is rebuilt. This is now in `doc/vm-control.md`
next to the test targets, which is where it will actually be read.

## Pipelined readahead's 500 ms figure is stale

`mmaptest` test 10 on `/var`, the number that justified pipelined readahead, is
**12 ms on a cold boot** as of 2026-08-12, not the ~500 ms on record: `fs::copy`
of `/bin/echo` 11 ms and `spawn+wait` 356 us, with `mmaptest /var` 11/11 in
37 ms. Whole-file prefetch covers anything under 2 MiB and `/bin/echo` is
329240 bytes, so the test never exercises the ramping window it was cited for.
The idea keeps its entry for the large-file case, and it now has an instrument:
`fsbench raprep /var`, reboot, `fsbench ra /var`. `doc/STORAGE-ROADMAP.md`
section 1b carries its baseline and states which readings decide whether a
change pipelined anything; `doc/fsbench.md` documents the mode.

## The guest boots `sata-disk.img`, and `make all` does not rebuild it

An hour of this went into believing a new `fsbench` mode did not exist: the
guest printed the *old* usage text for a binary that had just been built and
`make all` had reported success.

`make all` builds `programs`, the kernel and the ISO. It does **not** build
`sata-disk.img`, and that image — not the ISO's live root — is what the run
targets mount as `/`, because root selection prefers a real disk. Only the
`make run*` targets list it as a prerequisite, and `scripts/edos-vm start` does
not go through them, so a `make all` plus `scripts/edos-vm start` boots fresh
kernel against stale userspace. Nothing in the guest distinguishes that from the
change not working.

Leaving the image alone is deliberate: it is the persistent development root, a
rebuild is 5 GB and discards whatever the guest has written, and the manifest
guard exists precisely so a kernel edit does not trigger one (see the comment
above `update-manifest` in `GNUmakefile`). So the fix is not to rebuild it
automatically. `scripts/edos-vm start` now compares its mtime against
`filesystem/.manifest` and warns, naming `make sata-disk.img`.

## Counts, remeasured 2026-08-12

Every number a doc states about the size of the tree, taken rather than carried
forward. Remeasure before quoting one; the commands are here so the next reader
does not have to invent them.

| | value | how |
|---|---|---|
| syscalls | 111 | `grep -c 'const SYS_' kernel/src/syscalls/mod.rs`, and the dispatch arms and `table.rs` entries agree at 111 — a mismatch is the bug |
| userspace programs | 104 | `members` in `programs/Cargo.toml`, less `edos_lib` and `edos_render` |
| in-kernel test suite | 51 | `make test AUDIODEV=none` |
| `iotest /var` | 20/20 | the syscall regression suite, run in the guest |
| `unwrap()`/`expect()` in `kernel/src` | 205 | `grep -rIno --include='*.rs' -e '\.unwrap()' -e '\.expect(' kernel/src \| wc -l` |

The `unwrap` figure includes 11 in `thread/sched_test.rs`, which is test code and
not worth converting. By file, the ones that would move the number are
`drivers/usb/xhci/mod.rs` (19), `fs/efs/mod.rs` (8), `drivers/usb/hid/report.rs`
(8), `drivers/ahci/port.rs` (8) and `acpi/mod.rs` (7). Twelve of the xhci ones
are `Option` fields that `init()` fills, so removing them means folding `init()`
into `find_and_init()` rather than rewriting call sites.

## Naming a uid without a passwd database

`id` and `whoami` were listed as blocked on "users and file permissions", and
only half of that was true. `SYS_GETUID`/`SYS_GETGID` (102/104) already answer
from `UserThreadInfo.user_id`/`group_id`, which every process inherits from
`edos-init` and which nothing can change — there is no `setuid` and, per the
charter, deliberately will not be one until something can enforce it. So the ids
are real; what is missing is only a way to spell them.

That spelling is `edos_lib::process::id_name`, a table of the one identity the
kernel hands out (`0` → `root`), and it is the single place to replace when an
`/etc/passwd` exists. There is no `/etc` in `FILESYSTEM_DIRS` (`GNUmakefile`),
so nothing reads a database today. An id with no entry prints bare — `whoami`
prints the number, `id` omits the `(name)` suffix — rather than inventing one.

`chmod`/`chown` stay blocked, and on the other half: attributes are readable
(`FstatEntry::attrs`) but no `FileSystem::set_attrs` exists to write them back.

## The connection reaper unbound the listener, and a half-open lived forever

Two defects in the same 40 lines of `tcp_retransmit_main`
(`kernel/src/net/stack.rs`) and the passive-open path above it.

**The listener was unbound by its own connections closing.** The reaper collected
`c.local_port` from every connection it reaped and did `pt.remove(&(6, port))`.
A connection born of `accept` carries its *listener's* port, so the first
`TIME_WAIT` to expire took `(6, 23)` out of the port table with it and every
later SYN was answered with RST. `socket::unbind_port` had already been written
for exactly this — it removes only when the table's entry is the socket being
closed — but the reaper predated it and released by port number alone. It now
collects the owning socket alongside the port and applies the same `Arc::ptr_eq`
test. A dead `Weak` owner means the socket was closed and unbound on the syscall
path, so there is nothing to release.

Worth knowing how this hid: a single request per boot passes, and so do several
in a row, because the reaper only strikes once the connection leaves `TIME_WAIT`
five seconds later. It takes **two connections more than five seconds apart** to
see it. Iteration 7's httpd test was one `curl`, which is why item 7 looked
closed with this underneath it.

**A half-open connection was immortal.** The SYN-ACK was built inline with
`tcp::build` and sent once, so it was on no retransmit queue: a lost SYN-ACK was
never resent, and a peer that vanished after its SYN held a backlog slot until
the listener closed. `TcpConnection::build_syn_ack` now mirrors `build_syn` and
queues the segment, which buys both halves from machinery that already existed —
resend with RFC 6298 backoff, and death by `check_retransmit`'s `retries >= 5`
arm at about 63 s, which RSTs and marks the connection `Closed`. The reaper then
drops it and prunes the listener's `accept_queue` of any queued socket whose
connection went `Closed` without ever reaching `Connected`.

**A `SynReceived` half-open cannot be produced through slirp**, so do not spend
an iteration trying. QEMU's `hostfwd` terminates the host TCP connection itself
and then opens its own to the guest, which it always completes; the guest never
sees a handshake that stalls after its SYN-ACK. Exercising the deadline for real
needs a tap backend with a packet filter that drops the final ACK, or an in-guest
raw-socket test. What was verified in the guest is the listener surviving five
connections spread across 20 s with `netstat -a` showing one `TIME_WAIT`, the
`LISTEN` row intact, and no stranded `SYN_RECV`.

---

## The first inbound connection after boot was lost in the ARP cache

`send_ip` used to build the frame only when `arp_cache.lookup` hit, and return
`Err("arp pending")` otherwise. The packet was gone: the ARP request went out,
the reply arrived, and nothing remembered what the request had been for. Every
caller that could not block (the SYN-ACK path in `handle_ipv4`, the FIN paths in
`pipe.rs`, `io.rs` and `syscalls/mod.rs`, retransmits) simply dropped its
segment, which is why the first `curl` after boot failed and `netstat -a` showed
a stranded `SYN_RECV` with Send-Q 1.

`ArpCache` now holds one packet per unresolved target (`queue_pending_tx` /
`take_pending_tx`, RFC 1122 §2.3.2.2), flushed from `handle_arp` when the reply
lands. Newest wins per target and the map is capped at 16 targets, so a peer
that never replies costs one packet, not a growing queue.

Consequences worth knowing:

- `send_ip` returns `Ok(())` for a packet that has not reached the wire. That is
  the honest contract for a best-effort layer, and it made three ARP-retry
  loops dead: `syscall_ping`, `sys_connect` and `sys_sendto` each used to wait on
  an ARP waiter and re-send. All three are gone, and with them
  `ArpCache::get_or_create_waiter` and the `pending` waiter map.
- A cold-cache ping now measures ARP resolution inside its RTT, since the echo
  request leaves when the reply lands. Linux reports a first ping the same way.

Verified in the guest with `httpd -p 23 &` and one `curl` from the host through
`hostfwd tcp:127.0.0.1:2323`: 200 in 10 ms on the *first* connection after boot,
and the pcap shows the order — inbound SYN, `who-has 10.0.2.2`, the reply, then
the SYN-ACK 39 µs later.

`scripts/edos-vm start --pcap FILE` was added for that, since `make run-capture`
wants a local display and cannot run over SSH.

### The IPv4 id was always zero

`ipv4::build` hardcoded `identification = 0` while `next_ip_id()` was called and
discarded in `send_ping`. Fragment reassembly keys on that field, so two
concurrent fragmented flows to the same peer would have aliased. `build` now
takes the id and `send_ip_inner` supplies it. Still zero in one place: DHCP hand
-rolls its own IPv4 header (`net/dhcp.rs:176`) rather than calling `ipv4::build`,
which is harmless for a never-fragmented broadcast but is the last id=0 sender.

---

## A stop signal did not cut short a sleep, and `thread_sleep` was not why

Ctrl+Z on `sleep 30` took effect only when the 30 s were up. The mechanism on
record blamed `thread_sleep` for not returning early on a signal, and that is
wrong: `thread_sleep` already aborts on a wake, `transition_sleep` consumes the
wake token exactly as park does, and `kill_process_with_signal` wakes its target
before setting anything.

The loop is one layer up. `sys_nanosleep` (`kernel/src/syscalls/mod.rs`) sleeps
against an absolute deadline and re-enters `thread_sleep` with the time
remaining, so the early return did happen and the loop immediately undid it. It
checked `exit_if_killed`, which is why a kill got through and a stop did not.

The fix calls `stop_if_signalled` alongside `exit_if_killed` inside that loop.
The thread holds nothing there, which is the condition that doc comment names
for suspending a thread, so the suspension is safe in the middle of the call.
On `SIGCONT` the loop recomputes the remaining time and sleeps the balance. The
deadline is absolute, so time spent suspended counts against it: a `sleep 30`
suspended for a minute returns as soon as it is continued.

`sys_sleep_ms` needs no equivalent change; it does not loop, so its single early
return already reaches the syscall boundary where the stop is taken.

### The second half: `stop_if_signalled` parked once and the wake token ate it

With only that change the guest still waited the full 30 s. Instrumenting the
signal path settled it in one boot. Serial log, `sleep 30` interrupted at t = 2 s:

```
[25.599102] signal_process_group pgid=27 signum=20 members=[27]
[25.599104] kill pid=27 signum=20 state=Ready woke=true
[25.607726] nanosleep tid=27 woke, stop_requested=true
[25.607730] nanosleep tid=27 sleeping 27940 ms
```

Every layer worked: the line discipline signalled the right process group, the
default action set `stop_requested`, the wake claimed the sleeper, and the
nanosleep loop ran `stop_if_signalled` 8 ms later with the flag set. Four
microseconds later it was sleeping again — so the park inside
`stop_if_signalled` returned without ever parking.

`thread_park_while` documents exactly this: it **may return spuriously**, because
`transition_park_while` consumes the wake-pending token and bails when it finds
one, and it deliberately does not loop internally (looping would re-park without
re-enrolling on a wait queue, which breaks the wait-queue protocols). The token
here is the one `do_wake` published to deliver the signal: `try_wake` claims a
`Sleeping` thread through the state machine and never clears it, so it survives
into the *next* park the thread attempts. `stop_if_signalled` called
`thread_park_while` exactly once, so that stale token turned the suspension into
a no-op and the syscall resumed.

The fix loops on the condition around the park, which is what every other
`thread_park_while` caller in the tree already does (they are all bodies of
kthread `loop`s). `stop_if_signalled` enrolls on no wait queue, so re-parking is
safe there.

Verified in the guest: Ctrl+Z on `sleep 30` returns the prompt at once and the
next command runs, while an unsignalled `sleep 5` still takes its five seconds.

Ruled out along the way, so do not re-derive them: a stale ISO, the wrong
syscall, the loop lacking a `stop_if_signalled` call, `signal_process_group`
failing to match, and the wake failing to reach the sleeper.

**Generalisation worth carrying:** a stale wake token is left behind by every
wake that ends a sleep or a park, so any *single* `thread_park_while` call is a
latent no-op. Treat the one-shot call as the bug, not the token.

---

## Fixed: the shell read one byte and called it a character

Typing the Spanish ISO `ç` and redirecting it wrote `c3 83 c2 a7` instead of
`c3 a7`. Nothing was encoding twice; the shell's readline decoded once, wrongly.

`read_line` in `programs/edos-sh/src/main.rs` reads stdin one byte at a time and
did `let ch = ch as char`. That cast is a Latin-1 decode, not a UTF-8 one: it
takes the byte as a code point. `0xC3` becomes `U+00C3` and re-encodes as
`c3 83`, `0xA7` becomes `U+00A7` and re-encodes as `c2 a7`, and the two together
are exactly the sequence observed. Every layer below was already correct — the
terminal widget collects `Vec<char>` and writes real UTF-8, and the PTY line
discipline passes bytes through untouched — so the bug was entirely in the
one-byte-at-a-time reader treating each byte as a whole character.

The fix reads the rest of the sequence when a byte at or above `0x80` arrives,
using `utf8_seq_len` for the expected length, and decodes with
`str::from_utf8`. A stray continuation byte or a lead byte RFC 3629 no longer
permits is dropped rather than inserted, so a malformed sequence cannot corrupt
the line buffer.

Verified in the guest: `echo ç > /var/k.txt` then `hexdump /var/k.txt` gives
`c3 a7 0a`, with `ç` echoed correctly on screen. Use `/var`, not `/tmp`: memfs
reads past EOF and pads the last page with zeros, so a hexdump there shows
trailing garbage that has nothing to do with the write. `hexdump` here takes no
`-C`.

Sending the key: `scripts/edos-vm key backslash`. `ç` is `OEM7` in
`programs/edos_lib/src/keymap.rs`, which is the ISO key beside Enter.

---

## The floor, measured with the host quiet

Every number in this file older than this section was taken while the host was
doing something else, and the previous session concluded from a 4x spread that
nothing under 30% could be attributed. That spread was the host. This is a VM:
when the host has a build or a test suite to run it deschedules the whole vCPU,
which the guest cannot see and which looks exactly like slow code.

Five consecutive `switchbench` runs, single-CPU boot, nothing building on the
host (a resident Ethereum devnet was still running, about 2.5 of 12 hardware
threads -- this machine has no truly idle state). Median, with the spread
across the five runs:

| | ns | spread |
|---|---|---|
| `sched_yield`, nothing else Ready | 285 | 283-288 |
| `sched_yield`, handover to a sibling thread | 340 | 328-350 |
| `sched_yield`, handover to another process | 505 | 499-533 |
| `getpid` | 94 | 92-95 |
| `read` of a descriptor that does not exist | 128 | 128 |
| a pipe write + read, nothing blocking | 387 | 384-537 |
| a blocking pipe round trip | 2016 | 2009-2036 |
| the same round trip, one address space | 1808 | 1800-1812 |

The last three rows were re-taken on 2026-08-12 after the wait-queue work below
(402 / 2203 / 1988 before it); the rest have not moved since they were first
measured.

**The same binary now repeats to within 2%**, which is what makes a 25 ns
change measurable. The rule that follows is simple: do not measure while
anything is building, and read the median of five runs.

These decompose cleanly, which the old noisy numbers never did:

- the syscall boundary is **94 ns**, and the fd table another **34** on top;
- a switch and its trampoline are **~190** (`sched_yield` 285 minus a `getpid`);
- the pipe's own work is **~73 ns per call**.

`/proc/sched_prof` says 220 for the switch alone, which is *more* than the whole
285 minus 94. Its probes are two `rdtsc` reads per stage boundary, so its stages
rank the parts of a call and do not add up to one. That is a caveat this file
did not have before, and it matters: see the memset trap below.

## There was no 3-microsecond gap: it was the benchmark

A blocking pipe round trip read **2203 ns**, not the ~4900 this file and
`SCHED-ROADMAP.md` reported for months, and the 3.7 us that "nothing could
account for" was never the kernel. Every figure below is that 2203 ns baseline;
the round trip reads 2016 ns today, for the reason the end of this section gives.

`switchbench`'s `pipe_round_trip` timed one batch of 2000 trips with no warmup,
while every other figure it prints is the best of six batches after 64 warmup
iterations. A `fork`ed child starts with every page copy-on-write, so that single
unwarmed batch charged the round trip for the faults of its own child starting
up. Measured the same way as everything else:

| | ns/round trip | spread |
|---|---|---|
| cross-process | 2203 | 2196-2206 |
| one address space (a thread at the far end) | 1988 | 1983-1992 |
| **the address space, per switch** | **~108** | |

**That ~108 ns agrees with the yield path**, where a cross-process handover costs
129 ns more than a same-process one. Two independent measurements of the same
quantity that now agree, where they used to differ elevenfold. That is the reason
to believe these and not the old ones.

**How it fooled a whole round of analysis.** The thread-vs-process comparison was
added to find where the missing microseconds went, and it *did* isolate them --
onto the address space, which is where the difference in method happened to sit.
The conclusion (a `CR3` reload costs ~1470 ns in TLB refills, amplified by nested
paging) was wrong, and the next piece of work chosen on the strength of it, huge
pages for user mappings, would have bought nothing measurable.

What killed it is now a permanent part of `switchbench`: both round trips can
touch a 32-page working set per side per trip, and doing so costs the same
whether or not an address space was switched in between.

| | 0 pages | 32 pages | delta |
|---|---|---|---|
| one address space | 1988 | 2280 | +292 |
| cross-process | 2203 | 2590 | +387 |

**~1.3 ns per page refilled after a `CR3` reload**, not the ~100 a nested walk
was assumed to cost. Refills are not where the time is, so nothing that reduces
them -- huge pages, and PCID if this host had it -- is worth building.

**Two rules out of this, both about measurement:**

- **Never compare a best-of-N-with-warmup figure against a single unwarmed
  batch.** Every case in one benchmark has to be timed the same way, or the
  difference between two cases is a difference in method.
- **A stable artifact is still an artifact.** The bad number reproduced to within
  8% across runs and across four builds, and that is exactly why it was trusted.
  What caught it was two ways of measuring one quantity disagreeing by 11x.

### What is left: ~620 ns per round trip

| | ns |
|---|---|
| 4 pipe syscalls, at 94 boundary + 34 fd table + ~73 pipe work | 804 |
| 2 switches, at ~230 plus ~108 when the address space changes | 676 |
| 2 wakes (`do_wake`) | 102 |
| **accounted** | **~1580** |
| **measured** | **2203** |

~310 ns per park/wake pair, and the suspect was the predicate: the blocking read
performs a **whole** read attempt before it blocks, and `wait_internal` then
evaluated its predicate up to three more times, with a queue push and a `retain`
around them. Unlike everything this section retracts, that is a cost bare metal
pays too.

**Two of those three evaluations are gone (2026-08-12): 2203 -> 2016 ns.**
`WaitQueue::wait_until_unready` drops the entry check for a caller that has just
established the condition is false under the real lock, and the tail check was
dead for every untimed waiter because both of its branches returned `Parked`.
The same commit gave `WaitQueue` an exact `waiters` count so `wake_one` and
`wake_all` cost nothing when nobody is enrolled. The ordering argument that
makes the count safe is in `doc/SCHED-ROADMAP.md` section 1 — it is the one part
of this worth reading before touching the queue, since a relaxed read there is
precisely the missed wakeup in `doc/bugs/2026-04-13-sched-park-wake-missed-wakeup.md`.
What is left of the ~620 is the third evaluation, inside `transition_park_while`,
which is what makes the park safe and cannot be removed.

### What survived: the kernel half was only global for what existed at boot

The one measured win against the address-space switch, and it stands:
a cross-process `sched_yield` handover **506 -> 456 ns**, of which the
address-space part went **177 -> 128**.

`mark_kernel_mappings_global` sweeps the kernel half once, at boot. Everything
mapped there afterwards was non-global, including a thread's kernel stack and the
per-CPU scheduler stack the voluntary switch pivots onto -- the two regions every
syscall and every switch touch. `map_memory` now sets `GLOBAL` on any kernel-half
mapping itself, so the next site cannot forget. The controls did not move (thread
handover 328 ns, same-address-space round trip unchanged), which is what a fix to
post-`CR3` work should look like. Freed kernel stacks keep their mapping, so no
global entry outlives what it maps; where a kernel mapping is torn down, `invlpg`
and the `CR4.PGE` toggle both ignore the `G` bit.

## The pipe and the PTY share one ring now, 480 ns to 402

`Pipe::read` allocated a `Vec` for the bytes it drained and then memmoved the
remainder with `drain(..n)`; `sys_write` allocated another to stage the user's
bytes before taking the pipe lock; `sys_read` took that lock three times, once
to clone `reader_wq`, once to drain, and once inside every `wait_until`
predicate. Both directions of the PTY did the same thing to every keystroke and
every character a program printed.

All of it is one `ByteRing` now (`kernel/src/util/ring.rs`), behind the pipe and
both PTY directions. It grows to fit, keeps its allocation, and restarts at zero
whenever it drains, so two processes passing single bytes settle on one buffer
and never allocate again. Reads fill a caller-provided buffer, so no device
allocates on a read; the non-blocking path takes the lock once, and `reader_wq`
is only fetched when a read really has to park.

A pipe write plus read went **480 -> 402 ns**. Verified by hashing 1.1 MB
through a pipe (`cat /bin/switchbench | sha256sum` matches `sha256sum
/bin/switchbench` byte for byte), `wc -c` reporting the exact size, `$(ls /bin)`
capturing a multi-kilobyte substitution, heredocs, and 51/51 in-kernel tests --
`byte-ring` in `sched_test.rs` is the new one, and it exercises wrapping and a
growth that has to linearise a wrapped ring.

### The trap: a stack buffer costs its declared size, every call

The staging buffer that replaced the per-call `Vec` is a stack array, and **Rust
zeroes a stack array**. So the first version swapped an allocation for a memset
of the array's full size on every call, whatever the transfer, and at 2048 bytes
the two cancelled exactly:

| staging buffer | one-byte pipe echo |
|---|---|
| a `Vec` per call, as before | 480 ns |
| 2048 B | 480 ns |
| 512 B | 455 ns |
| 128 B | 404 ns |

128 B shipped: enough for a byte of IPC or a keystroke, with anything larger
taking the heap where one allocation is amortised over a copy worth making.

Two things worth keeping from how this was found. The end-to-end number said
"no change" while `/proc/sched_prof` said `pipe_copy_out` had gone from 9 ns to
135 -- **the compiler had moved the memset across a probe boundary**, because
the boundary is an `rdtsc` and nothing stops code crossing it. And the honest
reading of "no change at all, to the nanosecond" was that something new had been
added of exactly the size of what was removed, which is what it was.

### Fixed on the way: a wait predicate that could panic the kernel

`WaitQueue::wait_until` evaluates its predicate inside `without_interrupts`, and
its doc says so: a predicate that takes a contended `BlockingMutex` there trips
that primitive's interrupts-enabled assertion. The pipe's read predicate took
the pipe lock, and the PTY slave's took the PTY lock. Both now probe with
`try_lock` and treat a contended device as ready, which is safe because the loop
around the wait re-checks under the real lock either way. It only fired under
contention, which is why it survived this long.

---

## The context switch round: 1917 ns to 433

Both columns here were taken while the host was busy. The floor section above
re-measures the "after" column on a quiet host and gets 285 ns for the idle
yield rather than 433; the *ratio* is what this section is about, and it
survives.

| | before | after |
|---|---|---|
| `sched_yield`, nothing else Ready | 1917 ns | 433 ns |
| `sched_yield`, handover to a sibling thread | 1832 ns | 490 ns |
| `sched_yield`, handover to another process | 2215 ns | 751 ns |
| pipe round trip between two processes | 9429 ns | 4552 ns |
| the switch itself, `/proc/sched_prof` | 1270 ns | 220 ns |

Three changes, in the order they matter.

**The APIC timer is armed only when what is already armed will not do.**
`context_switch_to` re-armed the one-shot on every switch to push the incoming
thread's slice out, and that write — one x2APIC store to `IA32_TSC_TMICT`,
trapped by the hypervisor — was 1024 ns of a 1270 ns switch. A timer already
set to fire *earlier* than the new deadline satisfies it; only one that would
fire late forces the write. `expire_timeslice` already compared each thread
against its own deadline and let an early tick pass, and `tick_finish` re-arms
for what is left of the slice, so a thread still gets all of it and just takes
one extra tick to notice. Yielding in a loop now costs one interrupt per
timeslice instead of one trap per switch.

This is what a tickless kernel's clock-event layer does, and why Linux ships
`HRTICK` — an hrtimer armed at the exact slice end — turned off by default.

**`FS.base` moved to `rdfsbase`/`wrfsbase`**, following the `HAS_FSGSBASE`
gate `per_cpu.rs` already had for GS. 104 ns to 34.

**Kernel mappings are `GLOBAL` now, and `CR4.PGE` is on.** Neither had ever
been set, so a `CR3` write discarded the kernel's own translations along with
the outgoing process's and the next syscall re-walked them. The kernel half is
the same page tables in every address space, so the bit is exactly true of
them. 2805 leaves marked at boot; it is worth nothing to a same-address-space
switch and a fifth of a cross-process one.

### Two things that were tried and are not worth doing

- **`XSAVEOPT` instead of `FXSAVE`.** Measured: save 32 → 36 ns, restore
  59 → 83 ns. It can only win by *skipping* components, and with `XCR0`
  holding x87 and SSE there are none to skip — it saves the same registers
  `FXSAVE` does and adds a 64-byte header plus per-component work. Its
  modified optimisation needs consecutive `XSAVEOPT`/`XRSTOR` on the same
  area, which two threads handing off to each other never do. It becomes the
  right answer only if `XCR0` grows something large and optional (AVX and
  wider) that most threads leave alone. The note lives above `save_fpu_state`.
- **PCID.** Not possible on this machine: the host is a Ryzen 5 5600, and Zen
  3 has no PCID, so `qemu` refuses `+pcid` with "host doesn't support requested
  feature: CPUID.01H:ECX.pcid". It cannot be exposed to the guest or tested
  here. Global kernel pages above are the part of the same win that is
  reachable; what remains — a process's own translations dying on every switch
  — needs the hardware.

### What is left, with numbers

The switch is 220 ns and the whole `sched_yield` is 433, so roughly 210 ns is
now the syscall boundary, the trampoline and `iretq`. Inside the switch:
`page` 66-77 (an `RwLock` read and a `CR3` read even when the address space
does not change), `fxrstor` + `fxsave` 91, `CpuContext` copies 36, publish 19,
transition 27, `wake_sleepers` 18, pick 12, timer 10.

`switch_to_page` is the next cheap one: it takes `user.read()` and reads `CR3`
before deciding it has nothing to do. Mirroring the thread's `CR3` in an
atomic would remove the lock; there are only three sites that set it
(`thread.rs` thread creation, `execve`, `fork`).

But the bigger number is elsewhere. **A pipe round trip is 4552 ns and each
park/wake is 2276, against 490 ns for a yield handover** — so the wake
machinery costs about four times the switch it performs, and it is what every
real workload here pays: a shell pipeline, the compositor, the terminal.
Nothing has profiled it.

**`doc/SCHED-ROADMAP.md` is where the next round is written down**, in priority
order with the evidence and the outside references: a minimal voluntary switch
in the shape of Linux's `__switch_to_asm`, an L4-style direct handoff to the
receiver on a blocking IPC, spin-then-park, and the two small items above.

**PCID is not coming to this machine.** It is not an old feature — Intel has
had it since Westmere in 2010 — but AMD did not ship it for a decade, and this
host, a Ryzen 5 5600 (Zen 3, Vermeer), does not expose it: no `pcid` in
`/proc/cpuinfo` (it does have `invpcid`), and `qemu -cpu qemu64,enforce,+pcid`
refuses with "host doesn't support requested feature: CPUID.01H:ECX.pcid".
Reporting at the time had Zen 3 adding it on the **EPYC** parts. Do not plan
around PCID here without checking the CPU first.

## A context switch is one MSR write and a rounding error

Measured on a single-CPU boot with `switchbench` (userspace, end to end) and
`/proc/sched_prof` (kernel, stage by stage; `--features sched-prof`):

| | ns |
|---|---|
| `sched_yield`, nothing else Ready | 1917 |
| `sched_yield`, handover to a sibling thread | 1832 |
| `sched_yield`, handover to another process | 2215 |
| pipe round trip between two processes | 9429 |

**The first two numbers settle a question the last round left open.** A
`sched_yield` on an idle CPU returns to the *same* thread, so it was recorded
as a floor that might be hiding the real cost of a handover. It was not: a
genuine two-thread handover costs the same, within noise. An address-space
switch on top adds ~380 ns.

Inside the kernel, one switch is 1270 ns and this is where it goes:

| stage | ns |
|---|---|
| `set_apic_timer` | **1024** |
| `switch_to_page` (`CR3`) | 67 |
| `fxrstor` + `fxsave` | 93 |
| `FS.base` read + write | 104 |
| `CpuContext` copies, both sides | 40 |
| publish, transition, pick, wake_sleepers | 76 |

`set_apic_timer` is **81% of a context switch.** It is one x2APIC write to
`IA32_TSC_TMICT`, which KVM traps and answers by re-arming a host timer, and
`context_switch_to` does it unconditionally on every switch to push the new
thread's timeslice deadline out.

Everything the previous round nominated as a lever is real but small: no PCID
is 67 ns of directly visible cost (the refill misses it also causes are not in
this figure), the unconditional 512-byte `fxsave`/`fxrstor` pair is 93 ns, and
the `CpuContext` copies under a spin `Mutex` are 40 ns. `FS.base` goes through
`RDMSR`/`WRMSR` for 104 ns while `CR4.FSGSBASE` has been on since boot and
`rdfsbase`/`wrfsbase` cost a cycle or two.

### How to take these numbers again

`programs/switchbench` is the end-to-end side and **must be run on a
single-CPU boot** — give the scheduler a second CPU and it puts the two
threads on both, where neither ever waits for the other and every yield case
collapses back into the idle one. It prints the CPU count it saw for exactly
that reason.

`/proc/sched_prof` is the breakdown, and reports cumulative work rather than a
rate, so a measurement is: read the file, run the workload, read it again,
subtract. The probes only exist under `--features sched-prof`, which must be
passed to the **ISO** target rather than the kernel target.

```bash
make edos-x86_64.iso CARGO_FLAGS="--features sched-prof"
scripts/edos-vm start --smp 1
scripts/edos-vm type 'cat /proc/sched_prof > /tmp/b.txt; switchbench 20000 -l; \
    cat /proc/sched_prof > /dev/klog; cat /tmp/b.txt > /dev/klog' --enter
```

## Fixed: a single-CPU boot never flushed its own TLB

`make run-single` could not reach a desktop: `edos-taskbar` and
`edos-terminal` both took a #GP inside `edos_rt`'s allocator within 50 ms of
starting, and `edos-init` gave up on them. Four CPUs were fine, which is the
wrong way round for a race.

`munmap` unmaps each page with `flush.ignore()` and flushes the range once at
the end — and that final flush was guarded on `shootdown_needed()`, which was
`cpu_count() > 1`. With one CPU online the range was never invalidated at all,
so freed frames went back to the allocator while the faulting CPU still held
live translations to them. `tlb_shootdown` already skips the IPI round when it
is alone, so the guard never saved anything and cost the local flush.

`shootdown_needed` is deleted; every unmap path calls `tlb_shootdown`
unconditionally. Full writeup in
`doc/bugs/2026-08-11-single-cpu-skipped-its-own-tlb-flush.md`.

## The clock was the most expensive thing in the kernel

`Instant::now()` was an MMIO read of the HPET main counter. QEMU emulates the
HPET in its own userspace, so every read was a full exit to the hypervisor:
**6361 ns measured**, against **16 ns** for `rdtsc`. There were 72 call sites,
including one on each side of every context switch and one per AHCI command.

`Instant` now holds **nanoseconds, not counter ticks**, so a value stays
comparable across a change of source; `tick()`/`from_tick` are `as_nanos()`/
`from_nanos()` and every `*_tick` field was renamed. The TSC is only used when
`CPUID.80000007H:EDX[8]` reports an invariant TSC, each AP re-checks itself
against the HPET at bring-up and demotes everyone on disagreement, and
`clocksource=hpet` on the kernel command line forces the old behaviour. QEMU
does not advertise invariant TSC unless asked, so the run targets pass
`+invtsc`; under TCG the bit is absent and the HPET is kept automatically,
which is the right answer since TCG's TSC is counted instructions.

| | before | after |
|---|---|---|
| clock read | 6361 ns | 16 ns |
| `sched_yield` | 20818 ns | 1357 ns |
| `poll`, 1 idle fd, timeout 0 | 13877 ns | 330 ns |
| 512B raw device reads | 13.6 MiB/s | 35.3 MiB/s |

Two defects only a fine-grained clock exposes, both fixed here:

- **`set_apic_timer` clamped the initial count to 1 tick, not the duration.**
  Writing 0 stops the one-shot timer permanently, so a floor existed — but one
  tick at Div1 fires before the handler that armed it returns. A deadline that
  has just arrived now asks for a nearly-zero timer routinely. Floored at 10 us
  and saturated instead of truncated at the top.
- **A slow clock was an accidental rate limiter.** Anything that read it in a
  loop got a free backoff. Nothing depended on that in the end, but it is the
  first thing to suspect if a spin loop starts misbehaving.

**`poll` never consults the clock for a zero timeout now**, and reads it once
rather than twice for a timed wait. That, not the allocations, was the cost the
whole time: the two clock reads a timed call made were **12.7 us** against
**158 ns** for the entire per-descriptor path.

### …which promoted the per-descriptor allocations, and they are gone now

With the clock reads gone, the 158 ns marginal cost of a descriptor was worth
attacking. A poll call used to allocate **2 + 2N** times; it now allocates 2 or
3 regardless of how many descriptors it watches:

- **One `PollSet` for the whole call** replaces `Arc<PollWaiter>` plus an
  `Arc<PollEntry>` per descriptor. A device holds a `PollRef`, which is a
  refcount on the set plus an index, so registering costs no allocation. Its
  slots live inside the set for eight descriptors or fewer.
- **No `Box<dyn Pollable>`.** The pollable is built on the stack to register
  through; only descriptors the device actually kept a registration for are
  named again, as a `PollTarget` enum holding the `Arc` the descriptor table
  already gave us. Only the filesystem path still boxes.
- **A descriptor that registers nothing gets no context at all.** Its readiness
  is frozen at registration, so it is written into the caller's array once and
  counted there.
- `Vec<SelectFd>` is a stack array for eight descriptors or fewer.

Medians of three runs, before and after:

| n | ready before | after | idle before | after |
|---|---|---|---|---|
| 1 | 636 | 347 | 329 | 318 |
| 4 | 796 | 696 | 663 | 568 |
| 16 | 2569 | 1841 | 2374 | 2005 |
| 64 | 10175 | 6408 | 10213 | 7696 |

**Marginal cost per descriptor: 158 → 99 ns.** Fixed cost is unchanged at
~162 ns, and the descriptor snapshot stays on the heap: `FileDescriptor` is
about a hundred bytes, so an inline array of them costs more to initialise and
drop than the allocation it saves.

**Two measurement traps this produced, both of which I acted on before catching:**

- `pollbench` used to price the allocations as the gap between an invalid
  descriptor and `stdout`. That is wrong: in a terminal, fd 1 is a **PTY
  slave**, not `StandardStream::Stdout`, so it takes the PTY lock and registers
  like any other device, and the terminal is redrawing during the measurement.
  The "82 ns for two allocations" that figure produced was never real.
- Single readings of the fixed-cost line swing between 150 and 256 ns. A
  three-run median says the fixed cost did not move; one run said it had
  regressed by 75 ns. The `n >= 2` rows are stable to a few percent and are the
  ones to trust.

---

## lstat is real now, so `is_symlink()` finally answers

`std::fs::symlink_metadata` used to follow links: `lstat` in the fork's
`library/std/src/sys/fs/edos.rs` was literally `stat(p)`, so
`Metadata::is_symlink()` was always false and every path-based link test through
std was dead code. Programs worked around it by calling `readlink` and treating
success as proof of a link.

The whole chain now exists:

- `fs::api::file_info_nofollow` resolves with `LinkMode::NoFollow`, so only the
  final component is left unresolved — leading components are still followed,
  which is what POSIX.1-2024 specifies for `fstatat`.
- `sys_fstatat` accepts `AT_SYMLINK_NOFOLLOW` (0x100) instead of refusing every
  non-zero flag. The `FstatEntry` wire format already had `kind == 2` for a
  symlink and EFS already reported it; nothing but the resolution was missing.
- `edos_rt::fd::lstat_path` (0.0.43) goes through `SYS_FSTATAT` with `AT_FDCWD`,
  because `SYS_STAT` has no argument for the flag.
- The fork's `lstat` calls it.

The `readlink` workarounds in `ls` and `stat` still work and were left alone;
new code should use `symlink_metadata` instead.

## Warnings are a gate now, and one of them was load-bearing

Both builds are warning-free. Getting there turned up something worth knowing
before the next cleanup: **`cargo check` on the default features reports items
as dead that the `sched-test` feature uses.** `Thread::set_affinity_mask` is
the example — its only caller is `queue_spawn_kthread_affine`, which is behind
`#[cfg(feature = "sched-test")]`. Deleting it on the strength of the default
build's warning would have broken `make test`. It carries
`#[cfg_attr(not(feature = "sched-test"), allow(dead_code))]` now. Run
`cargo check --features sched-test` before deleting anything the lint calls
dead.

Two warnings were vestigial code whose doc comments claimed consumers that
never existed: `Transaction::ring_blocks` said it was "set by seal_and_commit"
and nothing ever set it, and `ReplayResult::ring_blocks_consumed` said the
caller used it to initialise `head_block` when the caller takes that from the
on-disk journal superblock. Both are gone. The rest are `#[allow(dead_code)]`
with a reason: `FaultReject`'s payload fields are read only through the derived
`Debug` in the `KILL: PF ... reject={reject:?}` line, which the lint does not
count, and `MAP_FIXED`/`MS_INVALIDATE` are unimplemented flags that still
document the ABI's flag space.

## Three defects the overnight run's own code review missed

Found reviewing the 2026-08-11 run, all fixed in the same commit:

- **`sys_listen` could leak a port for the rest of the boot.** The socket lock
  is dropped before the port-table insert (correct — the receive path takes them
  the other way round), then re-taken to set `listening`. A close landing in
  that window ran its own `unbind_port` before the entry existed, so nothing
  ever removed it: `sys_bind` refuses the port from then on, and an arriving SYN
  finds a listener that is not listening. The `EBADF` path unbinds now.
- **`/proc/<tid>/fd` reported a live thread as missing.** It reads the
  descriptor table through `try_lock` — which it must, because a thread reading
  its own `fd` file can already hold that lock further up the syscall path — but
  folded the contention case into the same `None` that means "the thread
  exited", which the read path turns into `FileNotFound`. `PROCESS_FILES` now
  returns `Result<String, Error>`, contention is a bounded spin and then `Busy`.
- **`destroy_windows_for_pid` could name a destroyed window as the focus heir.**
  It accumulated the last non-`None` heir, so a process whose window W1 handed
  focus to its own W2 before W2 was destroyed with nothing left to inherit
  returned W2. It reads `focused_window` once at the end instead.

## `waitpid(WUNTRACED)` is level-triggered here, and that is deliberate

Worth knowing before someone else "fixes" it. POSIX.1-2024 reports a stopped
child "whose status has not yet been reported", once; this kernel answers for as
long as the child is down, so the same suspension is reported on every call.

That reads like a bug and is not. Two callers depend on it as a state query:
`programs/sigtest` stops a child, sees the stop, waits, and asks a *second* time
to prove the child did not resume on its own; and `edos-sh` polls the same way.
A latch that consumed the first report was written and reverted after sigtest
hung on it — the second query blocked forever instead of answering. The comment
in `sys_waitpid` says so now.

---

## Start here for anything about storage performance

`programs/fsbench` measures the filesystem across idioms and depths: a memory
filesystem, a raw block device, and EFS. **Do not benchmark storage by hand or
write a one-off test — run it.** It also verifies what it wrote, and prints the
delta of every relevant `/proc` counter, which is what turns a number into a
diagnosis.

```bash
fsbench -l /var              # EFS: writes, reads, metadata, verify
fsbench -l raw /dev/sda      # the block layer and AHCI ceiling
fsbench -l rawwrite /dev/sdX # destructive; refused on a mounted device
fsbench -l /tmp              # memfs: the syscall and copy ceiling
```

`-l` mirrors the report to `/dev/klog`, which lands in `run_log.txt` on the
host — the guest terminal is far too short to hold a full run.

- [`fsbench.md`](fsbench.md) — how to run it, what each number means, and the
  record of what the 2026-08-09 round found and fixed.
- [`STORAGE-ROADMAP.md`](STORAGE-ROADMAP.md) — what is worth doing next, in
  order, with the evidence for each, **and a list of five experiments that
  measurement refuted.** Read that list before optimising: two of them sounded
  obviously right and made the system slower.

Three traps that round produced, all of which made a number mean the opposite
of what it said:

- A throughput figure is meaningless unless you know whether the work was
  deferred. A buffered `write` returns at page-cache speed; only the `fsync`
  rows and `sync()` measure the disk.
- Reading back in the same boot reads the page cache. Cold numbers need
  `fsbench write`, a reboot, then `fsbench read`, which is also what
  `scripts/fs-regression` does for durability.
- **Comparing two builds under the default time budget measures the wrong
  thing.** The faster build does more work per test, so it meets every later
  test with a fuller, more fragmented filesystem. Two sides of the clocksource
  comparison allocated 176927 and 221918 blocks and reported 483 against 1.8
  MiB/s for `mmap store 4MiB + msync` — while at a fixed `-n 32` operations
  both had a **1.0 ms median** and differed only in their worst single
  operation. Use `-n` for any A/B, and rebuild `sata-disk.img` between runs.

---

## The big change: the OS is now driven by an agent, not by hand

`make run` needs a local display, which is useless over SSH. `scripts/edos-vm`
boots the same ISO headless and exposes two channels: VNC for a human, and QMP
for scripts. QMP gives screenshots as PNG, synthetic keystrokes, and pointer
events, so the whole desktop can be driven and observed from outside the guest.

Read [`vm-control.md`](vm-control.md) before touching it. Three guest properties
will otherwise waste an hour: the keymap is Spanish ISO, the mouse is HID boot
protocol so absolute pointing is silently ignored, and the window manager
focuses on click so keystrokes go nowhere until you click into a window.

This immediately paid for itself: ten minutes of scripted input found a
whole-GUI deadlock that manual use had never hit, because nobody clicks that
fast for that long.

---

## Fixed and verified on hardware

- **User virtual address space is reused.** `find_free_address` was a monotonic
  bump allocator that never reclaimed anything, burning ~940 MB of address space
  per 9.2s on an idle desktop against 2.4 MiB of live mappings. Now a first fit
  over the VMA tree. Stride fell to 8-10 MB and successive mmap/munmap cycles
  return the same address.
- **`sys_window_list` no longer holds a spin guard across a user copy.** A user
  copy can demand-fault and park, and parking with a spin guard live stops every
  other CPU. It now snapshots under the guard and copies outside it.
- **Filesystem errors keep their errno.** `sys_list_dir` and `sys_open`
  flattened everything to EINVAL despite a correct `From<FsError> for Errno`
  existing. Missing paths report ENOENT now.
- **`make filesystem` creates the directories it claims to.** It used brace
  expansion, and make runs recipes under dash, so it silently created one
  directory literally named `{bin,dev,home,...}` and `/var` never existed.
- **`OpenOptions` opens files for writing.** `read`, `write`, `truncate` and
  `create_new` were no-op stubs in the std fork, so every file was read-only as
  far as the kernel was concerned. This is why `mmap(MAP_SHARED, PROT_WRITE)`
  failed. Fixed in the fork as commit `88d827604b3`, on `origin/edos_std_v3`.
- `sha256sum` and `file`, two Phase 3 userspace programs.

`mmaptest` went from failing at test 1 to all 10 passing on both `/var` and
`/tmp`.

**`VfsInode::drop` no longer panics the kernel on the reaper.** The drop-contract
guard asserted that the drop never *runs* on the reaper or evict kthread, but the
contract is that it never *blocks*, and the whole point of posting to the evict
kthread is to make the reaper path safe. The reaper frees a dead thread's FDs and
VMAs, so it routinely releases the last reference to an orphaned inode:
`mmaptest`'s unlink-while-mapped test panicked the kernel on trunk. The guard now
sits on the one blocking path, the queue-full fallback in `post_evict`, where the
reaper gives the eviction up (counted as `dropped_count` in `/proc/evict_stats`,
reclaimed by `efs-fsck`) instead of stalling teardown behind disk I/O. `mmaptest`
now passes 10/10 on both `/var` and `/tmp` with no panic.

**`make test` is green for the first time, and covers more**, 47/47 (was 30).
Added: the preemption counter's nesting and balance, `BlockingMutex` mutual
exclusion under contention, `BlockingRwLock` reader sharing plus writer
exclusion, and `WaitQueue::wake_all` releasing every waiter. Each was checked
against a deliberately broken build first — the mutex test reports 500 of 2000
increments when the guard is dropped across the read-modify-write, and the
waitqueue test strands three waiters when `wake_all` is swapped for `wake_one`.
Both handshakes are counter-based rather than timed: an earlier version waited
only for the queue to become non-empty and flaked about once in twenty. It was red on trunk: the
`abort-race` test called `thread_park_while` bare and treated any return as a
completed round, which is the exact contract violation
`bugs/2026-04-13-sched-park-wake-missed-wakeup.md` warns about. It now loops on
its condition, and the waker counts a round before releasing the parker so the
final count is not a race. Run it as `make test AUDIODEV=none` from a bare SSH
login: the default `pipewire` backend has no session bus to talk to there, and
QEMU refuses to start rather than falling back.

---

## The GUI deadlock was a scheduler bug, and is fixed

**A window-registry reader wedged the whole GUI**, with all four CPUs spinning
on `WINDOW_REGISTRY.write()`. Full writeup in
[`bugs/2026-08-08-window-registry-stuck-reader.md`](bugs/2026-08-08-window-registry-stuck-reader.md).

The holder was never parked. It was **`Ready` and starved**: the register dump
only proves it was not *running*, and the scheduler could pass over a runnable
thread forever. Two defects made the wait unbounded, both now fixed:

- **The timeslice was armed but never enforced.** `context_switch_to` set
  `slice_deadline` and armed the timer to it, but `maybe_preempt` bails unless
  `NEED_RESCHED` is set and nothing set it on expiry; `slice_deadline` was read
  only by procfs. A thread was preempted only when another became runnable on
  its CPU. `Scheduler::expire_timeslice` now marks it.
- **Anti-starvation only covered wake-boosted threads.** `pop_next` reached
  `pop_lower_than` only when `rq_boosted` was set, which happens for
  `WakePriority::Interrupt` wakes alone, so a high *base* priority thread
  starved everything below it. It now counts every pick and services the highest
  non-empty lower level every `STARVE_STREAK_LIMIT`.

The window-input kthread runs at priority 10 and user threads at 7, so a
preempted guard holder behind that kthread was never picked again. The same
hazard applied to every spin lock shared across priorities, including `VFS`.

`starvation-victim` in `thread/sched_test.rs` is the regression test: one
CPU-bound spinner per CPU above `DEFAULT_PRIORITY` plus a default-priority
thread whose progress the spinners sample across the saturated window. With
either fix disabled the victim advances by exactly 0; with both it advances by
~800k.

The reader instrumentation is still there and still useful, since it names the
holder rather than its state:

```bash
make edos-x86_64.iso CARGO_FLAGS="--features window-lock-debug"
scripts/edos-vm start
scripts/window-lock-soak 3000
```

Slots decode as `(tid << 8) | site`. `WINDOW_REGISTRY_READER_ACQUIRES` is the
positive control: live slots last microseconds, so an empty table only means
something if that counter is moving. It reads about 259/sec on an idle desktop.
Having named a holder, read its `State`: `Ready` and `Parked` have completely
different causes.

On top of the scheduler fix, spin locks shared between threads now suppress
preemption for the guard's lifetime (`thread/preempt.rs`): a per-CPU counter
that `maybe_preempt` honours, plus `PreemptSpinlock`/`PreemptRwLock`. Converted:
`WINDOW_REGISTRY` (280), `WINDOW_EVENTS` (290), `VFS` (10), `UserThread.vmas`
(70), `memory_manager` (80), `SHARED_MEMORY_REGISTRY` (90), the input
`Broadcaster` (310), and the thread registries.

Suppressing preemption rather than interrupts is deliberate: `memory_manager`
walks page tables and `vmas` walks the VMA tree, so disabling interrupts across
them would trade a scheduling problem for a much worse interrupt-latency one.
`thread_park*`, `thread_sleep` and `thread_yield` debug-assert that preemption
is enabled, which doubles as an automated audit for "spin lock held across a
park" — it stayed silent through boot, the stress tests and the FS paths.

Still bare, deliberately: the scheduler's own `rq`/`sleepers`/`SCHEDULERS` and
`WaitQueue.inner` (wrapping them would recurse into the counter), and the
IRQ-reachable locks that correctly use `IrqSpinlock`.

---

## Cross-repo state

The userspace allocator and the std fork both live outside this repo, and both
are now current. Two traps to know before you touch either again.

**The `edos_rt` clone can be behind crates.io.** 0.0.34 and 0.0.35 were
published from a tree that never landed in `github.com/edg-l/edos_rt`, so the
repo was two releases behind and a patch on top of it would have silently
reverted file-backed `mmap`, `msync` and the `OpenFlags` access-mode constants.
Diff the clone against the published crate before editing:

```bash
curl -sL -o /tmp/rt.crate https://crates.io/api/v1/crates/edos_rt/<max_version>/download
mkdir -p /tmp/rt && tar xzf /tmp/rt.crate -C /tmp/rt --strip-components=1
diff -ru /tmp/rt/src ~/dev/edos_rt/src
```

**The std fork's pin is the version that actually runs.** `library/std/Cargo.toml`
sat at `edos_rt = "0.0.26"` for ten releases while the crate moved on, and a
`0.0.z` requirement is exact, so none of that work reached any program. It is now
0.0.46. The full loop for an allocator or syscall-wrapper change is: patch
`edos_rt`, bump, `cargo publish`, bump the pin, `cargo +nightly update
--manifest-path library/Cargo.toml -p edos_rt`, `./x install` in `~/dev/rust`
(prefix `~/dev/edos-toolchain`, linked as the `edos` toolchain), then
`make programs`.

`PoolAllocator` fragmentation is fixed in 0.0.36: the free list is
address-ordered and coalescing, and the header records the whole reserved span
rather than the requested size. 0.0.37 then released idle chunks back to the
system, gave large allocations a header so alignment above a page is honoured,
and added a bounded cache of freed large mappings. `bench/allocstress` in the
`edos_rt` repo is the regression check; it compiles the allocator against a
shimmed `mmap` on the host and fails if the pool does not plateau, if freeing
everything does not hand the memory back, or if an over-aligned large request
comes back misaligned.

0.0.37 also carries the runtime fixes that came out of reading the rest of the
crate: the syscall wrappers are inlinable Rust-ABI functions instead of
`no_mangle extern "C"`, `thread_join` blocks in the kernel rather than polling
at 1 kHz, `getrandom` fills the whole buffer instead of returning a count std
discarded, `IoError` is the `Errno` itself so a caller can tell a missing path
from a full disk, and `Mutex` only enters the kernel when a waiter is actually
parked. `decode_error_kind` in the fork covers every `Errno` now, which was only
possible once the errno stopped being folded away below it.

0.0.38 followed, for two reasons that are worth separating from the bug below.
The allocator's own locks went back to a spin lock: its critical sections are a
few list operations long, so parking under them bought nothing and put a syscall
in the middle of a list walk, and the preempted-holder hazard that motivated the
change is bounded now that the kernel enforces the timeslice. The inline syscall
wrappers also stopped declaring the argument registers as merely read; a syscall
that parks resumes its caller through the scheduler rather than straight back out
of the entry stub, and `in(...)` promises the compiler those registers survive
that path too. They are `inout(...) => _`, which is what the out-of-line
`extern "C"` call implied before inlining.

Neither of those was the corruption. **Do not repeat the mistake of reading a
timing change as a fix**: the spin-lock build looked clean for several runs and
the futex build lost threads, which is what the difference in scheduling looks
like when the real fault is a narrow race elsewhere. The next section is the
actual cause.

---

## Fixed: concurrent mmap handed the same address to several threads

Corrupted memory in any multi-threaded program. Fixed by making the claim atomic;
kept here because the symptom sent two separate investigations into the allocator.

`bin/threadtest hammer` runs eight threads allocating hard. The serial log shows
three of them receiving the *same* mapping:

```
thread-75: mmap: lazy mapped at 0x143b000
thread-76: mmap: lazy mapped at 0x143b000
thread-73: mmap: lazy mapped at 0x144b000
thread-72: mmap: lazy mapped at 0x143b000
```

`sys_mmap` picks the address under one acquisition of the VMA lock
(`syscalls/memory.rs:115`, `vmas.find_free_address`) and inserts the `Vma` under a
separate, later one (`syscalls/memory.rs:212`). Two threads can therefore both run
the first fit, both see the range free, and both take it. The window is small; it
needs several threads calling `mmap` at once to hit.

The consequence is exactly the corruption that looked like an allocator bug: two
threads' `PoolAllocator` chunks alias the same pages, so one thread's free-list
links land in the other's blocks, and `alloc` then faults reading a link from an
address like `0x28`. Chasing it through the allocator wasted a lot of time, twice.

Worth knowing: `find_free_address` became a first fit over the VMA tree in the
same session that fixed the VA leak, and first fit **reuses** freed ranges, so a
stale pointer now lands in live memory instead of an unmapped hole. That makes
any aliasing far more damaging than it would have been under the old bump
allocator.

`VmaSet::reserve` now runs the first fit and inserts the VMA under the one
acquisition the caller holds, and `find_free_address` is private, because an
address it returns is only free while the lock is held. `syscalls::memory::
claim_range` is the single entry point; there were **four** call sites, not one:

- anonymous `mmap`
- file-backed `mmap`
- `MAP_PHYSICAL` `mmap`
- `sys_shm_map`
- the 2 MiB thread stack in `sys_clone` — the worst of them, since every
  `std::thread::spawn` goes through it, so two concurrent spawns could share a
  stack

The two paths that can fail after claiming (physical `mmap`, `shm_map`) release
the range on the way out. Widening the guard instead was the alternative, and was
rejected: `vmas` is a `PreemptSpinlock`, so holding it across the page-table work
would turn every mapping into one non-preemptible span, and anything added to
that span that can park would then be a bug rather than merely slow.

Verified over ten `threadtest hammer` runs (eight threads each) across two
builds: no address appears twice within one address space, no faults, no panics.
`mmaptest` (10/10), `threadtest` and `forktest` pass, and the in-kernel suite is
47/47.

Mind how you check for this. Duplicates have to be counted **per address space**,
which means segmenting the log by process and keeping only that process's own
threads. Two naive versions of the check both cried wolf on me: separate runs of
a program are separate address spaces, and `mmaptest` execs two copies of `echo`
that legitimately map at the same address.

## Fixed: a syscall could run with a kthread as the per-CPU current thread

`bin/threadtest` panicked the kernel once in roughly eight runs with

```
KERNEL PANIC: current_thread_info: no UserThreadInfo for tid 3
  src/thread/scheduler.rs:1162
```

on `cpu-2`, while a kernel thread was current. `tid 3` is a kthread, and kthreads
have no `UserThreadInfo`, so the lookup failed. Every caller of
`current_thread_info()` lives in `kernel/src/syscalls/`, so a syscall handler was
running while that CPU's current thread was a kthread.

The receiver was the bug. `current_thread_info` was a method on `Scheduler`, and
it answered from `self.current` — the field of **one CPU's** scheduler. Callers
wrote

```rust
let sched = sched();                     // the CPU we are on *now*
let info = sched.current_thread_info();  // ...answered by that same CPU, later
```

and a syscall runs with interrupts enabled (the entry stub does `sti`), so the
caller can be preempted between those two lines and resume elsewhere. The
`&'static Scheduler` then names the CPU it has left, whose `current` has moved on
to another thread — a kthread, in the panic above.

Which thread is current is a property of the CPU executing right now, so it is no
longer reachable through a `&Scheduler` at all. `current_thread`,
`current_thread_id`, `current_thread_weak` and `current_thread_info` are free
functions that read the per-CPU slot with interrupts off, which makes the read
atomic against migration; the `Arc` they return stays valid however the thread
moves afterwards. `Scheduler::current` survives as the private `running_tid`, for
the scheduler internals that legitimately ask "what is *this* CPU running" from a
context that cannot migrate.

**The rule to keep: `&Scheduler` never means "me".** It means one specific CPU's
run queue. Anything phrased as "the current thread" belongs to the free functions.

`thread_exit` had the same defect with worse consequences: it cleared
`self.current` and decremented `self.thread_count`, so a migration mid-call left
the *departed* CPU believing it was idle while it ran someone else. It is a free
function now and resolves its scheduler inside the interrupt-off window.
`thread_yield`, `thread_park`, `thread_park_while` and `thread_sleep` moved too;
they never touched `self`, and leaving them as methods invited the same mistake.

Two latent bugs fell out with it. `lock_order::enter` compared a per-CPU
`current_thread()` against a scheduler-derived `current_thread_id()` and would
have fired its single-owner assert on any migration between the two; both sides
now read the same source. The `window-lock-debug` reader table recorded the tid
of whichever CPU the guard was taken on, which is exactly the wrong tid for the
instrumentation whose job is naming a stuck reader.

Why it surfaced when it did: no program used `std::thread` before, so userspace
never had several runnable threads competing across four CPUs. `threadtest`
exists to keep exercising that.

---

## Fixed: a CPU stopped answering TLB shootdown IPIs, then double faulted

Reproducible in about a minute, which the `current_thread_info` panic never was.
Drive `threadtest`, `threadtest hammer` and `threadtest nojoin` in a loop through
`scripts/edos-vm` on a 4-core boot. Around t=52s the log turns into nothing but

```
<cpu-2:bin/edos-wm:u:21> tlb_shootdown: timeout waiting for CPUs (mask=0x1), forcing clear
```

repeating (314 times in the observed run), and the desktop stops responding to
input while the taskbar clock keeps redrawing. `mask=0x1` is CPU0, and CPU0 never
acknowledges again. Register dump at that point:

| CPU | RIP | state |
|---|---|---|
| 0 | `interrupts::idt::double_fault_handler` | halted |
| 1-3 | `Scheduler::run_idle` | halted |

So CPU0 wedged first, kept missing shootdown IPIs until it double faulted, and
the rest of the machine went idle behind it.

A second run wedged with a different tail, and that one named the cause: three
CPUs spinning in `IrqSpinlock::lock` on the serial port with interrupts off, and
the fourth spinning in `tlb_shootdown` waiting for their acknowledgement.

This is **not** related to the identity fix above: it reproduces identically on
the commit before it (`f51ab70`), and slightly sooner (t=52s vs t=76s, 6 vs 10
completed `threadtest` runs).

### Fixed: `IrqSpinlock` waited with interrupts disabled

`IrqSpinlock::lock` disabled interrupts and *then* spun for the lock, so a CPU
waiting on a contended one answered no IPIs for the whole wait — including TLB
shootdowns. Interrupts only need to be off while the lock is *held*, which is
what keeps an IRQ handler from deadlocking against the holder; taking an IRQ
while still waiting is harmless, because the waiter does not hold it yet. It now
disables, tries, and re-enables around a read-only spin on the contended line.

The serial lock is what made this bite. Every thread exit logs a line, every
UART byte is a VM exit under KVM, and `threadtest` spawns some forty threads a
run, so under the loop above the serial lock is saturated and CPUs sit in that
IF-off wait for far longer than the shootdown's 10M-iteration timeout.

Effect on the reproducer: **916 shootdown timeouts became 0**, and the machine
survives the full loop where it previously stopped logging entirely at t=52s.

### Root cause: an idle CPU squats on a thread's kernel stack

`run_idle` holds `context` — a pointer to the interrupt frame — in a local
across `enable()` and `enable_and_hlt()`. On the timer-preemption path that
local and that frame both live on the **outgoing thread's kernel stack**,
because `timer_interrupt_handler` never pivots RSP. The voluntary path does the
opposite, and the comment on the scheduler-stack allocation in `init` says why:
it pivots "so the outgoing thread's kernel stack is completely free before any
waker can resume it".

By the time `pick_and_run` reaches `run_idle`, `maybe_preempt` has already run
`save_current_thread` (setting `context_saved = true`) and enqueued the thread,
so any other CPU may steal it and resume it *on that same kernel stack* while
this CPU is still idling on it. Two CPUs then write one stack, and the squatting
lasts as long as the CPU stays idle.

Caught with `--features trace` on a 10-core boot, first iteration of the loop:

```
cpu 0:  [36] Save   cpu=0 tid=46 rip=0x412cd9
        [37] Switch cpu=0 46->50
cpu 9:  [13] Steal  0->9 tid=46
        [14] Switch cpu=9 0->46 rip=0x412cd9     <- from_tid 0: CPU 9 was idle
```

CPU 9 panicked in that switch with `cw: Low context address 0x1` — its
`context` local had been overwritten while it idled. The same mechanism explains
the double faults and the impossible interrupt frames seen earlier, where
`instruction_pointer` held a plausible RFLAGS value (`0x286`) and `code_segment`
an index of 6400 against a seven-entry GDT.

### Fixed: leave the thread's stack before publishing the thread

Two paths kept using a kernel stack after handing the thread to somebody else.
Both now pivot to the per-CPU scheduler stack first, which is the discipline
`save_transition_switch` already followed and documented.

**`thread_exit` is the one this workload hammered.** It called
`reaper_enqueue(t)` and *then* `switch_away()`, which does `sub rsp, 160` and
calls into Rust — on the dying thread's kernel stack, the stack `Thread::free`
unmaps. The reaper runs on another CPU, so it may pull that stack out from under
the exiting thread at any point after the enqueue. `threadtest` exits roughly
forty threads per run, which is why it reproduced there and nowhere else.
`thread_exit` now only marks the thread `Dying`; `switch_away` pivots, and
`reap_and_schedule` posts to the reaper and picks the next thread from the
scheduler stack.

**The timer tick had the same shape.** `context_switch_to` writes the incoming
thread's frame into a frame sitting on the *outgoing* thread's stack, after that
thread has been enqueued and can already be running elsewhere. `on_tick` is
split into `tick_prepare` (thread stack; saves the outgoing context, returns the
stack to pivot to) and `tick_finish` (scheduler stack; enqueues and picks), with
the naked handler copying the 160-byte frame between them. `CpuContext` gained a
const assert on that 160, since three trampolines hard-code it.

Verified on a **10-core** boot, the configuration that previously died on the
first iteration: 25 iterations of the `threadtest` / `threadtest hammer` loop, 50
clean completions, 697 threads spawned and reaped, no panic, no double fault, no
shootdown timeout, no garbage in the log, every CPU idle afterwards. 47/47
in-kernel tests.

**A warning about reading the evidence here.** An intermediate build had only the
tick pivot, and it failed with the log prefix itself garbled
(`<cpu-633166472:kernel>`, uptime near `u64::MAX`). That looked like the pivot
had broken the GS-based per-CPU pointer. It had not: `_serial_print` formats on a
thread's kernel stack, so the still-unfixed exit path was corrupting the logging
path's own locals. Corrupted output names where corruption *landed*, not what
caused it — the same trap as the serial log ending mid-line in the wedges above.

### Fixed: the shootdown timeout acknowledged flushes that never happened

Separate from the stack bug, and wrong regardless of how often it fired.
`tlb_shootdown`'s timeout force-cleared `pending_mask` and returned, on the
reasoning that "the lagging CPUs will flush redundantly when they eventually
process the IPI, which is safe". It is not safe. Returning tells the caller that
no CPU holds the old translation, and the caller is entitled to free or reuse
the page on the strength of that; a CPU that never acknowledged is still reading
through the stale entry. The escape hatch traded a stall for silent corruption,
and the 314-timeout run above was doing exactly that, 314 times.

Three things were wrong and all three are fixed:

- **Giving up at all.** The wait now re-sends the IPI to the CPUs still
  outstanding and, if `ACK_ATTEMPTS` rounds pass with no acknowledgement,
  panics naming the mask and range. A wedged CPU is a bug worth stopping for;
  continuing is not a recovery, it is corruption with the evidence discarded.
- **Acknowledgements that credit the wrong round.** `pending_mask` was reused
  across rounds with nothing distinguishing them, so a late handler from a
  timed-out round could clear a bit for the round in flight — reporting a flush
  it never performed. A `generation` counter is bumped per round; the handler
  captures it before flushing and only acknowledges if it still matches.
  Skipping is safe: a round still waiting on that CPU has an IPI latched for it.
- **The initiator could be descheduled holding `active`.** Every other CPU
  wanting a shootdown spins on that flag, so the round now runs with preemption
  suppressed, per the rule in `thread/preempt.rs`.

Re-sending is cheap insurance rather than the main point: an IPI to a CPU with
interrupts off is latched and will fire, so a re-send only helps if one was
genuinely lost.

### Fixed: `pick_sched` sampled `thread_count` twice

Not part of the stack or shootdown bugs; found by soaking for them. `pick_sched`
made one pass to find the minimum `thread_count` and a second to find a
scheduler matching it. Other CPUs spawn and exit throughout, so every count can
rise above the sampled minimum in between, the second pass matches nothing, and
it reaches `unreachable!()`. It now takes one pass keeping the best sample,
starting at the rotation offset so the round-robin tie-break is unchanged.

Worth noting how it turned up: a soak that mixed `mmaptest` into the
`threadtest` loop, because `mmaptest` spawns a child of its own and roughly
doubled the spawn rate. Varying the workload found a bug that repeating the same
one never would.

---

## Audit, and the logging that came out of it

`doc/AUDIT.md` is a read-only pass over the whole tree: correctness, perf,
missing syscalls, smells, plus a list of things that looked like findings and
were checked and discarded. `ideas.txt` carries the prioritised follow-up.

One item is already fixed, because it was on every hot path. The kernel logged a
line per mmap, munmap, spawn, ELF load and thread exit. Each costs a `String`
allocation on the calling thread, and the drain side writes to the UART a byte
at a time under a global lock — one VM exit per byte under KVM. That is the same
serial lock whose saturation starved TLB shootdowns before `IrqSpinlock` stopped
waiting with interrupts off, so this was not a cosmetic cost.

`log_debug!` reads a relaxed atomic before formatting, so a disabled site costs
one load and no allocation. It is off unless the kernel command line carries
`loglevel=debug`, which makes it a dial rather than a rebuild. Failure paths
stayed on `log!`. Six `threadtest`+`hammer` iterations went from dozens of lines
each to **zero**; one `threadtest` with `loglevel=debug` still emits 37.

Two traps worth knowing if you touch this:

- **`ParsedCmdline::parse_str` allocates**, so reading the log level has to
  happen *after* `init()` brings the frame allocator up. Putting it before
  panics at `frame_allocator.rs:24` before serial is useful.
- **The serial log is no longer a way to count work.** Greps like
  `bin/threadtest:u:.* exit: code=` return nothing by default now. Use the
  terminal output, or boot with `loglevel=debug`.

---

## Fixed: an unvalidated address reached a VMA insert

Audit item 1.1 was that the ELF loader builds a mapping out of
`base_addr + p_vaddr` without ever bounding it, and that `VmaSet` applies
`USER_VA_END` only to addresses it picks itself. The audit could not say how bad
that was without building a crafted ELF.

No crafted ELF was needed. `sys_mmap` reaches the same insert with a raw user
address, and validated nothing beyond `length != 0`:

```
mmap(addr=0x0000_9000_0000_0000, len=0x1000)   # non-canonical, from any program
  -> claim_range -> VirtAddr::new
  -> KERNEL PANIC: virtual address must be sign extended in bits 48 to 64
```

Reproduced on a pre-fix kernel and resolved through the backtrace to
`syscalls/memory.rs:234`. Every VMA a process holds becomes a `USER_ACCESSIBLE`
mapping, so the canonical-but-kernel-half case (`0xffff_8000_…`) was the worse
half of the same hole: it does not panic, it inserts.

The check belongs in `VmaSet::insert`, which now returns `Result` and rejects a
range that wraps or ends past `USER_VA_END`. Callers that hand back a range the
set already held — unmap rollback, fork's deep copy, the TLS region the kernel
derives from `USER_STACK_TOP` — call `insert_validated`, which debug-asserts
instead; they have no error to report and no untrusted input. The loader bounds
the segment with `checked_add` before constructing a single `VirtAddr`, and
rejects `p_filesz > p_memsz`, which would otherwise push the file-backed VMA
past the end that was checked.

Two neighbours fell out of the same read:

- **`find_free_address` had the same bug in its align-up.** `(length + 0xfff)`
  wraps for a length within a page of `u64::MAX`, so it returned a gap far
  shorter than requested. Now `checked_add`.
- **Address-space exhaustion was an `expect`.** It reports `VmaError::NoSpace`
  (ENOMEM) instead of panicking.

`mmaptest` test 11 is the regression test: five cases (non-canonical, kernel
half, straddling the top, wrapping length, unsatisfiable length), each of which
must come back as a failed `mmap`. 11/11 on both `/var` (EFS) and `/tmp`
(memfs), 47/47 in-kernel tests, forktest and threadtest clean.

---

## Fixed: CPU affinity was a field, not a rule

`cpu_affinity` had a setter and one enforcement point. `thread_can_run_here` was
stubbed to `true` with the real check commented out beneath it, and
`complete_wake` enqueued on the waker's CPU without consulting it, so a pin held
only until something woke the thread.

Affinity is a **placement** property in this kernel: `spawn_thread`,
`complete_wake` and work-stealing pick the CPU, and `pick_and_run` runs whatever
it pops without re-checking. That is the cheaper design and it is now the
documented one — `set_affinity_mask` says a mask set on a running thread applies
at its next placement, not immediately.

The trap worth knowing: **un-stubbing `thread_can_run_here` alone would have
lost threads.** `spawn_thread`'s `else` arm was a bare comment claiming the
thread "will be queued on its target cpu by that cpu's scheduler", and nothing
did. The stub returning `true` is the only reason that arm never ran. It routes
through `pick_sched_for` now, and a mask naming no registered CPU runs the
thread here rather than dropping it.

Two notes on the test, because the first version of it was worthless:

- **Yields do not test affinity.** They re-enqueue on the CPU the thread is
  already on, so a thread that reached the right CPU by luck stays there. The
  first `affinity-pinned` passed with `allows_cpu` hardcoded to `true`.
- **Wakes do.** `complete_wake` prefers the waker's CPU, and the waker is
  elsewhere most rounds, so the pin only survives a wake if the check is real.
  With `allows_cpu` reverted the test dies on round 0: "pinned to cpu 3, ran on
  cpu 2 after wake 0". Any future change here should be checked the same way —
  revert the predicate, confirm the test fails.

A third thing fell out of the same read: `pick_sched` called
`schedulers.iter().nth(idx)` per candidate (audit 2.4), now one `cycle().skip()`
pass.

47 → 49 in-kernel tests, all passing, desktop and mmaptest/threadtest clean
afterwards.

---

## The rest of the audit, and two things it got wrong

Shipped: `clock_gettime` off the RTC (sampled once at boot, pinned to the HPET,
nanoseconds since the epoch); path syscalls on a stack buffer via
`copy_user_path`; `pread`/`pwrite` and `getuid`/`getgid`; the five bare
`spin::Mutex` sites on `PreemptSpinlock`; an RFC 6298 retransmit timeout for
TCP; `fs/api.rs` returning `ProtocolMismatch` instead of panicking.

Two audit recommendations were **checked and rejected**, which is the part worth
remembering:

- **CLOEXEC has nothing to govern.** There is no `exec` in this kernel. `spawn`
  builds a fresh process and gives it exactly three descriptors; `fork` copies
  the table, which is what fork does. No `O_NONBLOCK` exists either, so
  `F_SETFL` would set nothing. The flag becomes real the day `exec` lands.
- **`setuid` without a permission model** is a privilege change that enforces
  nothing. `getuid`/`getgid` are in; the setter is not.

Two bugs fell out of writing the tests rather than out of the audit:

- **`sys_read` held the fd-table `BlockingMutex` with interrupts disabled.**
  `sys_write` and `sys_close` clone the Arc, enable interrupts, then lock;
  `sys_read` locked inside the `UserThreadInfo` `IrqSpinlock` scope. Eight
  threads doing positional reads through one shared descriptor tripped the
  contended-with-interrupts-off assert, and the spinning then starved a TLB
  shootdown into its timeout. The same shape as the `IrqSpinlock` bug from
  earlier in the session: *the assert fires on contention, so a rarely-contended
  wrong lock looks fine for months.*
- **TCP cannot connect at all**, and never could — a pre-session build fails
  identically. `doc/bugs/2026-08-08-tcp-connect-rsts-its-own-synack.md` has the
  packet capture and the instrumentation results. It stayed hidden because
  `http`/`wget` use `std::net::TcpStream`, which the std fork does not
  implement, so nothing had ever completed a connection.

The RFC 6298 work is therefore correct by inspection but **unverified end to
end**; it cannot be exercised until a connection can reach Established.

---

## execve exists now

`execve` (59) replaces a process image in place, with `fcntl` (72) and
`FD_CLOEXEC` alongside it. The shape that matters, because it is what makes the
operation safe:

1. **Copy argv/envp/path out of user memory first.** The address space holding
   those strings is about to be unmapped.
2. **Build the new image in a fresh address space while the old one is live.**
   A load failure then returns an error with the process untouched, which is
   what POSIX requires of a failed exec. Only after the load succeeds is
   anything destroyed.
3. **Quiesce the siblings.** `address_space_refs` reaching 1 is the proof that
   no other CPU can touch the space, because a thread only decrements it in
   `Thread::free`, after it has stopped running.
4. **Detach the old space and attach the new one in a single step, then tear
   the old one down.** `context_switch_to` reloads CR3 from `user.cr3` on every
   switch, so freeing a page table that is still published there hands a
   preemption a dangling CR3. Detaching first also means the blocking part of
   teardown (a `MAP_SHARED` writeback reaches the disk) happens with nothing
   half-swapped.

Three things underneath had to change, and are worth knowing independently:

- **`Thread::new_user` is split.** `load_process_image` builds an address space
  and an image; `new_user` attaches a new thread to it; `execve` attaches an
  existing process to it. The loader/process seam used to be welded shut.
- **`Thread::free`'s teardown is now `release_mappings`**, over a detached
  `MemoryManager` and `VmaSet`, and its descriptor shutdown is
  `pipe::close_descriptor`, shared with exec closing its close-on-exec fds.
- **A killed thread now dies at the syscall boundary.** `killed` was previously
  read on exactly one path — a PTY slave read — so a thread doing anything else
  ignored it. Kill was, in effect, "kill a shell foreground job blocked on
  input". A thread that makes no syscalls at all is caught by the timer tick
  instead (see below); one spinning inside the kernel is still nobody's to kill,
  which is why exec bounds its wait and refuses rather than assuming.

**Two traps if you touch this:**

- **Sibling threads are not keyed by `UserThread`.** `sys_clone` gives each
  thread its *own* `Arc<RwLock<UserThread>>` sharing the inner Arcs, so
  `Arc::ptr_eq` on the `UserThread` matches nothing. Address-space identity is
  the `address_space_refs` Arc. Keying on the wrong one made the quiesce find
  no siblings and time out, and `exectest`'s multithreaded case is what caught
  it.
- **`exectest`'s wake cases are the load-bearing ones.** Cases 1-3 pass with a
  broken quiesce; only case 4 exercises it. Reverting the cloexec close alone
  fails case 2, which was checked.

---

## There is an init process now

`bin/edos-init` is the only thing the kernel starts. It supervises `edos-wm`,
`edos-taskbar` and `edos-terminal` with a thread each — spawn, `waitpid`,
restart with backoff, give up after five rapid failures — so which programs
make up a session is userspace policy rather than something compiled into
`main.rs`.

Two consequences worth knowing:

- **A binary that fails to load no longer panics the kernel.** `boot_load_thread`
  used to `unwrap_or_else(|e| panic!(...))`, so a broken `/bin` took the machine
  down. It logs and leaves the kernel up; if *init itself* will not load, that is
  logged loudly and the serial console still works.
- **Killing the window manager is survivable.** `kill <wm pid>` and init restarts
  it; the desktop stays usable and input keeps routing, because windows live in
  the kernel registry and the new WM adopts them. This was the outcome I was
  least sure of and it works — verified twice, with a shell command typed into a
  pre-existing terminal window afterwards.

### Parentage, and the exit-status leak

Threads now carry the id of whoever created them, and so do exit statuses.
Before this, every exit inserted a status into `EXITED_THREADS` and only
`waitpid` removed one, so any process nobody waited on leaked a record forever.
When a creator dies, its children's statuses are dropped (nothing can name them
any more) and its surviving children are handed to init. `/proc/processes` has a
PPID column and prints the pending-status count, which stays at 2 across dozens
of spawns.

**The trap, and it cost a debug cycle:** this bookkeeping must not run on the
exit path. A registry walk plus two `Vec` allocations there hung the scheduler
suite at 48/49 — the exit path can run with interrupts disabled, which is
exactly what `reaper_enqueue`'s "must not allocate" comment warns about. It runs
in the reaper now, and `record_thread_exit` takes the parent from the dying
thread the caller already holds, so it neither allocates nor takes a lock. If
you add anything to thread exit, assume no allocation and no locks until proven
otherwise, and run `make test` — the failure was a timeout, not a panic.

---

## TCP works now, and the bug was in the waitqueue

`WaitQueue::wait_until_timeout` slept once and returned on any wake, without
re-checking the predicate or the deadline. Since a wake token left by an earlier
wait aborts the next sleep, `sys_connect` — which waits for ARP and then for
Established, back to back — had its second wait return in microseconds, decided
it had timed out, removed the connection and returned ECONNREFUSED. The SYN-ACK
landed 0.2 ms later, matched nothing, and got an RST. **No TCP connection had
ever been established in this kernel.**

`sys_read`'s socket paths had the same bug at the call site: one `wait_until`,
then treat an empty buffer as EOF, so every read returned 0 bytes.

Both fixed; `doc/bugs/2026-08-08-tcp-connect-rsts-its-own-synack.md` has the
detail. Verified with `tcptest` against a host HTTP server: a 270367-byte
response arrives intact, which finally exercises the RFC 6298 retransmit work.
`ping` also stopped losing its first packet to the same spurious ARP timeout.

**Two things to carry forward:**

- **Do not make the untimed arm of `wait_internal` loop.** It looks like the
  obvious symmetry and it stalls the boot: a caller whose predicate only becomes
  true through work that same thread has yet to do never returns. Two of three
  services failed to start. This has now looked correct twice.
- **When a container appears to lose an entry, instrument every mutation before
  theorising about the memory model.** The first investigation produced a
  genuinely alarming table — same address, coherent neighbouring atomic,
  `len=1` in one thread and `len=0` in another — and every observation in it was
  accurate. What was missing was a trace on connect's own `remove`. A reader
  that disagrees with a writer is far more likely to be a third writer you have
  not looked at.

---

## Fixed: two mappings shared a page, and one zeroed the other

Any `Vec` grown past 64 KiB came back full of zeros with its length intact.
It surfaced as a networking bug — `wget` saved 0 bytes of a 300 KB file —
and was not one: `read_to_end` collected all 300204 bytes correctly, and the
search for the `\r\n\r\n` terminator then found nothing, because the buffer
had been zeroed underneath it.

`VmaSet::reserve` searched for a gap of `length` rounded up to a page and
then recorded the VMA with the raw `length`. `first_fit` starts its next
search at a VMA's `end`, so a mapping that ended mid-page put the next one
*inside that same page*, and either could then destroy the other: a
zero-fill fault installs a fresh frame, and `munmap` of one unmaps a page
the other still uses.

The allocator hits it on the first chunk that is not exactly `CHUNK_SIZE`.
Growing to 128 KiB maps a ~131136-byte chunk starting 65600 bytes into the
previous chunk's last page; the copy lands there, the old chunk is freed,
and `release_chunk` unmaps the shared page along with it.

This was newly *reachable*, not newly written: the old bump allocator
returned page-aligned addresses by construction, and first fit made the
cursor follow VMA ends instead. `reserve` and `first_fit` work in whole
pages now, `sys_mmap`/`sys_munmap` reject an unaligned address rather than
rounding one silently, and `vectest` grows a `Vec` to 2 MiB verifying every
byte after each step. Full writeup in
[`bugs/2026-08-08-mappings-sharing-a-page.md`](bugs/2026-08-08-mappings-sharing-a-page.md).

**Two things worth carrying forward.** A correct length says nothing about
correct contents: every layer here reported the right byte count. And when a
buffer is zero *from offset 0*, suspect its backing pages rather than its
writer — a writer that skipped work leaves a hole, an unmapped-and-refaulted
page leaves a zeroed prefix.

## Fixed: the cwd mutex was taken with interrupts disabled

`info.lock().cwd.lock()` reads as two locks taken in sequence and is not:
the `UserThreadInfo` `IrqSpinlock` guard is a temporary that lives to the end
of the statement, so the cwd `BlockingMutex` was acquired with interrupts
off. Eighteen call sites did this. It panicked the kernel during boot, and
the CPU that died then stopped answering TLB shootdown IPIs, so a second CPU
panicked behind it with "never acknowledged a flush".

The same shape as the `sys_read` fd-table bug earlier in the session, and the
same lesson: *the assert fires only on contention, so a rarely-contended
wrong lock looks fine for months*. `current_cwd` / `set_current_cwd` clone
the `Arc` out of the guard first, and every call site goes through them.

## std::net is implemented, and that is where sockets belong now

`http` and `wget` were ported onto `edos_lib` first, which worked but was a
workaround: `std::net::TcpStream` returning "unsupported" was the actual
defect. Every wrapper std needed already existed in `edos_rt`, and every
syscall behind them already existed in the kernel (socket, bind, connect,
listen, accept, sendto, recvfrom, shutdown, get/setsockopt, getpeername,
getsockname) — nothing was wired to std, so the target fell through
`cfg_select!` in `sys/net/connection/mod.rs` to the unsupported stubs.

`library/std/src/sys/net/connection/edos.rs` in the fork implements
`TcpStream`, `TcpListener`, `UdpSocket` and `lookup_host`. Options the
kernel really has are real (timeouts, linger, nodelay, ttl, `SO_ERROR`);
the rest report unsupported rather than lying, and IPv6 is rejected rather
than truncated. `http`, `wget` and `dns` are plain std programs again, and
`edos_lib::http` is gone.

Verified: a 300000-byte file over `std::net` hashes identically to the
host's copy, and `http edgl.dev` fetches a real page off the internet by
name.

**The toolchain loop has a trap.** Bumping the `edos_rt` pin and running
`./x install` rebuilt nothing — bootstrap did not notice the lockfile
change, reported success in 24 seconds, and userspace kept linking the old
std. `touch library/std/src/lib.rs` forces it. A build that finishes far
too quickly after a dependency bump has not done what you asked.

## Resolution, and the query that still fails

DNS lives in `edos_rt::net::lookup_a` now, behind `ToSocketAddrs`. The
parser it replaced existed in two copies and desynchronised on a name that
ends in a compression pointer after its labels (RFC 1035 4.1.4), reading
the pointer's first byte as a length; that is why `dns edgl.dev` failed
while `example.com` worked. It also reports *why* a lookup failed instead
of answering every failure with "no A record", and the kernel now keeps the
resolver address DHCP offered (`SYS_GETDNS`) rather than parsing it into a
field nothing read.

**The first DNS query after boot used to get no reply, and the cause was
not the ARP drop it looked like.** `sys_recvfrom` did a single
`wait_until` and returned zero bytes if the queue was still empty. That
call returns on *any* wake, so a token left by an earlier wait aborted the
park and the receive reported an empty datagram immediately. The `sendto`
that triggers ARP is indeed dropped, but a correct receive would simply
have waited for the retry's answer.

It also explains why the resolver's retry did not rescue it: every attempt
returned just as fast, so the third read the *second* attempt's reply and
rejected it on the transaction id — which is why the error was "malformed"
rather than "no A record", and why chasing the parser was a dead end.

This is the contract the TCP read path was fixed for earlier in the
session; `recvfrom` and `accept` were the two places that kept the old
shape. Both loop on the real condition now, and `recvfrom` honours
`SO_RCVTIMEO`, which `setsockopt` had been storing with nothing reading it.
Verified on four cold boots. `programs/dnsprobe` dumps a raw response if
this area needs poking again.

## Checking a downloaded file from inside the guest

**Watch out.** memfs reads
past EOF and returns zeros to the end of the last page, so `sha256sum` of a
file on `/tmp` hashes the padding too and never matches the host, while
`stat` and `cat` both look right. The same file on `/var` hashes correctly.
Tracked in engram (`engram-cli todo list`); verify downloads on EFS until it is fixed.

## Fixed: a port restart stranded the op it meant to fail

The AHCI watchdog entry in `ideas.txt` proposed gating `enter_ncq_mode` on
`AhciPort.restarting`. That gate is not the fix. It keeps *new* submitters
out of a port being reset, and the op that strands is already past it.

`fail_all_ncq_slots` skips a slot whose `issued` is still false, on the
grounds that the submitter's own post-issue path will notice the generation
change. But `reset_generation` was bumped at the *end* of `restart_port`,
after that pass. A submitter that stored `issued` between the pass and the
bump, and sampled `SACT` before the reset cleared it, saw an unchanged
generation and its bit still set — so it returned and waited for a
completion nobody would deliver. A watchdog sweep found it up to 30s later.

The generation is published in `begin_restart`, before the fail-all pass,
and the submitter re-reads it after storing `issued`. The orderings are
complementary: either the submitter observes the bump and completes its own
slot, or its store precedes the pass, which fails the op. The gate went in
too, as a throughput measure.

**How it was validated, which matters more than the patch.** A real NCQ
command against a qcow2 backing file completes in well under a millisecond,
so no sane watchdog timeout is ever reached and the race never occurs
naturally — a 30 ms timeout produced zero firings under load.
`ahci_ncq_timeout_ms=0` on the kernel command line instead makes a sweep
treat *every* in-flight op as hung, so restarts land inside submits at
whatever rate I/O is running. `/proc/ahci_stats` gained `stranded`, which
counts ops a sweep finds still pending from an earlier generation — the
bug's exact fingerprint, and zero by construction once the ordering holds.

Under forced restarts with mixed read/write load: **1 stranded in 33
restarts before the fix, 0 in 106 after**. The pre-fix rate would have
predicted about three. Keep the injection in mind for any future work on
this path; the default timeout is untouched at 30s and the knob is inert
unless the command line sets it.

## A kill now reaches a thread that never enters the kernel

`killed` was observed at the syscall return boundary, which covers every real
program and misses the one case that mattered: a thread spinning in user code
makes no syscalls, so nothing ever asked it to die. `execve` had to bound its
sibling quiesce and refuse with EAGAIN for exactly that reason.

The timer tick checks the same flag now, and the condition that makes it safe is
**ring 3 in the interrupted frame**. There is no unwinding here, so a thread that
dies holding a lock guard leaks it permanently — that is the reader leak in
`bugs/2026-08-08-window-registry-stuck-reader.md`. A frame from ring 3 proves the
thread held nothing; a tick that caught it inside the kernel is left to the
syscall boundary, where the same `exit_if_killed` runs. Both callers share that
one function, so there is no second copy of the rule to keep in step.

Placement is in `tick_prepare`, before the tick touches the runqueue: the thread
is still Running, nothing has been published, and `thread_exit` pivots off its
kernel stack the way it does from a syscall. EOI has already been sent by then,
which matters — checking earlier would leave the ISR bit set on a CPU that is
about to run somebody else.

Two tests, and both were checked against the previous kernel:

- **`programs/killtest`** signals a child in each mode. Test 1 spins in user
  code, test 2 blocks in a syscall. It hands off through a pipe rather than a
  sleep, because killing a child still inside its runtime's startup would
  exercise the syscall boundary whichever mode was asked for, and it polls
  `waitpid_nonblocking` with a bound, because the failure is a process that never
  dies and a blocking `waitpid` would report that as this program hanging.
- **`exectest` test 5** execs from a process whose four siblings spin without
  syscalls.

Without the check, killtest test 1 reports the child alive 1000 ms after the
signal and exectest test 5 exits `EXEC_RETURNED`; exectest 1-4 still pass, so
test 5 is the only one that depends on it. With it: killtest 2/2, exectest 5/5,
threadtest + hammer + forktest clean, mmaptest 11/11 on both `/var` and `/tmp`,
49/49 in-kernel.

Still not covered, deliberately: a thread spinning **inside** the kernel. Nothing
can kill that safely, so `execve` keeps its bounded wait and its EAGAIN.

## Fixed: five more guards live across a user copy

`sys_window_list` was one instance of a class, and the sweep for the rest of it
found five more. The rule the class breaks: **a lock guard must not be live
across a user copy.**

Why the copy is a park point, which is the part that is not obvious: in the
ring-0 branch of `page_fault_handler`, `handle_demand_fault` runs *before* the
uaccess fixup, deliberately, so that a `try_copy_*` touching a lazily-mapped
page gets it mapped instead of failing. That handler blocks — NCQ I/O,
block-page-cache shard contention, vma waitqueues — with interrupts re-enabled.
EDOS has no unwinding, so a thread killed while parked there never runs the
guard's `Drop` and the lock is held for the life of the machine.

| Site | Guard | Consequence of a kill there |
|---|---|---|
| `Pipe::{write_from_user,read_to_user}` | `BlockingMutex<Pipe>` | that pipe wedges; every reader and writer parks forever |
| `Pty::{master,slave}_{write_from_user,read_to_user}` | `BlockingMutex<Pty>` | the terminal wedges |
| `tty::write_from_user` | `TTY_BUFFER` | stdout dies for every process |
| `vfs::read_to_user` (non-page-cache path) | `inode.lock` read | that procfs/devfs inode is unreadable |
| `vfs::write_from_user` (non-page-cache path) | `inode.lock` write | that inode is unreadable *and* unwritable |

The fix is one shape everywhere: **buffer first, lock second.** Writes copy out
of user space before taking the lock; reads drain into an owned `Vec` under the
lock and copy out after dropping it. `copy_in`/`copy_out` in `syscalls/io.rs`
are the helpers. The pipe and pty types lost their `*_from_user` / `*_to_user`
methods entirely, which is what stops the pattern coming back: the types no
longer know what a user pointer is.

Two deliberate trade-offs, both narrower than the leak they replace:

- **A read that faults on the copy loses the drained bytes.** It used to copy
  first and drain only on success. A fault here means the caller passed a bad
  buffer, and the alternative (peek, copy, then drain under a second
  acquisition) lets two concurrent readers see the same bytes.
- **TTY writes longer than 256 bytes may interleave with another writer's**,
  since the buffer lock is now taken per chunk instead of for the whole write.
  A TTY makes no atomicity guarantee above that.

Checked and already correct, because they snapshot into owned memory first:
`sys_ioctl`, `sys_window_poll`, `sys_list_mounts`.

Verified on a headless boot: `echo hello | wc -c` → 6, `ls /bin | wc -l` → 70,
`cat /proc/meminfo | head -3` (the exact vfs fallback path that was fixed),
`dmesg | wc -c` → 7328 bytes through the rewritten pipe path, killtest 2/2,
exectest 5/5, mmaptest 11/11 on `/var`, 49/49 in-kernel, no panic and no
shootdown timeout in the log.

### The regression guard, and exactly what it covers

`lock_order::assert_no_guards_held` is called at the top of `thread_exit`.
Every path that ends a thread funnels through there, so it is the one place the
rule can be checked, and it costs an `is_empty()` on a debug build.

**It covers ranked locks only**, because that is what the per-thread stack
records. The three locks the sweep touched that were unranked are now ranked, so
all six sites are covered: `TTY_BUFFER` 210, `Pipe` 220, `Pty` 230.

Those ranks are pinned by two constraints, and the reasoning is in
`invariants/lock-order.md`. Above 30, because `/dev/tty0` is a devfs device and
devfs has no `PageCacheOps`, so writing to it runs `TtyDevice::write` under
`inode.lock`. Below 900, because appending to any of these buffers allocates and
a heap expansion reaches the frame allocator. Nothing ranked is acquired while
one of them is held, which is the property to re-check before adding anything to
those critical sections.

Proven in both directions, since an assert never seen to fire is decoration:

- **Negative:** 49/49 in-kernel, then killtest, exectest, threadtest, forktest,
  iotest, mmaptest 11/11 and `lockordertest: PASS (inversions=0, max_depth=4)`
  on a booted desktop, plus `echo x > /dev/tty0` and `cat /dev/tty0` to force
  the `inode.lock` → `TTY_BUFFER` ordering. `/proc/lock_order_stats` reported
  `inversions: 0` throughout.
- **Positive, twice.** Pushing a fake rank in the `SYS_EXIT` arm panics on the
  first program exit: `thread 27 died at thread_exit holding 1 ranked guard(s),
  innermost 'positive-control' (rank 10)`. Then, after ranking, holding a real
  `TTY_BUFFER` guard across the exit panics with `innermost
  'tty::positive-control' (rank 210)` — which is the proof the widened coverage
  is real and not just a bigger table. Both reverted.

Worth knowing when reading this class: **the hang that opened the entry in
`ideas.txt` was re-diagnosed as starvation**, not a leaked guard, by
`bugs/2026-08-08-window-registry-stuck-reader.md`. The class has never been
caught in the act. It was swept because the mechanism is provable by
inspection, not because that deadlock was an instance of it.

Where the rule can be broken at all is narrower than "anywhere a thread dies".
Every ring-3 kill point (GPF, invalid opcode, alignment check, page fault, the
timer tick) interrupts user code, where the thread provably holds nothing;
`exit_if_killed` runs after the syscall body returned and dropped its guards; a
ring-0 uaccess fault takes the fixup and returns EFAULT rather than killing.
That leaves explicit `thread_exit()` inside a syscall body, of which there are
two, both currently safe.

## Lock-order ranks now cover IPC, networking and the window system

Three subsystems ranked on top of the FS/MM ladder that Foundation #4 shipped:
TTY/pipe/pty 210-230, networking 240-270, window system 280-300. The rank table
in [`invariants/lock-order.md`](invariants/lock-order.md) is authoritative and
has the per-lock reasoning; this is the summary of what it bought.

**Networking was the payoff: two pre-existing AB/BA inversions**, both shaped as
"take the port table while holding something that belongs inside it".

- `tcp_retransmit_main`'s cleanup freed the ephemeral port inside the `retain`
  closure with the connection guard live, closing the cycle
  `PORT_TABLE -> SOCKET -> TCP_CONN -> PORT_TABLE`. It never deadlocked only
  because the socket held under the port table in `handle_tcp` is always a
  *listening* one, whose `poll_state` reads the accept queue instead of locking a
  connection. Nothing enforced that.
- `close_descriptor`'s socket arm took the port table under the socket guard,
  against the receive path's opposite order. This one needs no invariant to
  break: closing a listening socket while a segment arrives for it wedges two
  CPUs on preempt spinlocks — a syscall against the e1000e rx kthread.

Both now collect what they need under the guard and release it after. **Neither
is visible by reading either function alone**; the rank system found them
because no total order existed over the observed nestings. That is the argument
for doing this to a subsystem at all.

**The window system was already consistent** — no inversions. Worth recording so
nobody re-derives it: `handle_mouse_event` already drops its read guard before
upgrading to a write lock, `cleanup_process_windows` already scopes its guard,
and the event-queue side never reaches back into the registry.

**Ranking is also what makes a lock visible to `assert_no_guards_held`.** That
assert only sees ranked locks, so the pipe/pty/TTY ranks exist as much for the
dying-thread check as for ordering. If you add a lock that a syscall can hold
across a park, rank it even if it is a leaf.

Validation was the tracker itself, which panics on a wrong rank rather than
passing quietly: 49/49 in-kernel, a booted desktop with DHCP/ARP/ping/DNS and
repeated `http` fetches over the real internet, a synthetic click-and-type soak,
and `lockordertest: PASS`. `/proc/lock_order_stats` read `inversions: 0`
throughout. Note what that does *not* show: it proves the new order is
self-consistent under load, not that the old code would have deadlocked. The
case for both bugs is structural, from the code, not from a reproduction.

## USB, shared memory and the input path are ranked too

The follow-up sweep (2026-08-10) added ranks 204/206 (`Mailbox.queue`,
`ResponseInner.value`), 90 (`SHARED_MEMORY_REGISTRY`) and 310/320
(`Broadcaster.subs`, the `/dev/kbd` + `/dev/mouse` poller lists). No inversions
appeared; `/proc/lock_order_stats` read `inversions: 0, max_depth: 3` on a
booted desktop after mmaptest 11/11, forktest, lockordertest, and a window
opened and closed to drive the shm teardown path.

**USB has no locks of its own, and that is the result rather than a gap.**
`XhciController` is only ever `&mut self` inside its driver thread; the MSI-X
handler just wakes that thread. Every other thread reaches it through a channel,
so what the sweep actually ranked was the channels — a mailbox shared with the
FS mount path, and broadcasters shared with PS/2 input.

**The shm registry's old rationale was wrong, and that is why it stayed
unranked.** `invariants/lock-order.md` said it was never co-held with vmas (70)
or mm (80). That was an audit of `syscalls/shm.rs` alone: `sys_fork`'s deep copy
resolves each SHM VMA's region under the vmas guard, and `release_mappings` does
the same under the page-table guard, which `Thread::free` holds across the whole
call. Rank 90 sits inside both. The first attempt at 75 (inside vmas, outside
mm) is wrong for exactly that second reason.

**Two real defects came out of the USB half**, neither of them an ordering bug:

- `Broadcaster.subs` was a bare `spin::RwLock` shared between driver kthreads,
  the window input thread and syscall context — a descheduled holder stalls
  every other CPU, the shape of the window-registry hang. Now `PreemptRwLock`,
  and `subscribe` builds its 256-slot `ArrayQueue` before taking the guard
  instead of under it.
- The USB HID paths broadcast to subscribers but never notified pollers, while
  the PS/2 paths did. Since `USB_*_ACTIVE` suppresses the PS/2 producer, `poll()`
  on `/dev/kbd` or `/dev/mouse` never reported readable with a USB device
  attached — which is the default machine. Both halves now sit behind
  `dispatch_key_events` / `dispatch_mouse_event`.

**Audio and devfs finished the list (same day).** `HdaPlaybackState` is rank
330 and devfs's `DevFs.shared` is 340. Both were bare spin locks over
thread-shared state, the same primitive error as `Broadcaster.subs`: HDA's was
held across a memcpy loop into the DMA ring, between `/dev/dsp` writers and the
audio kthread. `TTY_POLLERS` also joined the device-poller class at 320.

**Ranking devfs paid for itself on the first `ls /dev`:**

```
lock order violation: tried to acquire 'tty::device_size' (rank 210)
while holding 'devfs::list_files' (rank 340);
full stack: [inode.lock(30), devfs::list_files(340)]
```

`read_bytes`, `write_bytes`, `ioctl`, `poll` and `mmap` all release the registry
guard before calling into a device. `list_files` and `file_info` did not,
because their call into the driver does not *look* like a dispatch:
`DeviceNode::file_entry` reads `DevFsDevice::size`, which for `/dev/tty0` takes
the rank-210 `BlockingMutex`. That is a spin lock held across a lock that can
park. Both snapshot the nodes under the guard and build their `File` entries
after it now. Ranking the registry *above* the device locks is what makes the
mistake loud; ranking it below would have been legal and silent.

**`scripts/edos-vm` had no audio device at all**, so `hda: no device found` and
the driver never initialized — the primary way this OS gets exercised could not
test audio. It now passes `-audiodev none,id=snd0 -device intel-hda -device
hda-output,audiodev=snd0`. `none` rather than `pipewire`: the guest DMA engine
and interrupts run either way, and pipewire refuses to start without a session
bus, which is the exact case that script exists for.

Still bare `spin::Mutex`/`RwLock` over thread-shared state, worth the same
treatment and not yet audited: the `log` ring buffer in `logs.rs` (careful, it
must stay reachable from paths that cannot take locks), `random.rs`'s RNG state,
`PCI_MANAGER`, and `ALLOWED_PHYS_RANGES`. The scheduler's own locks,
`PCI_CONFIG_LOCK` and the AHCI slot/mmio locks stay bare on purpose.

**A trap worth naming: rewriting lock calls mechanically can drop a `!`.**
Wrapping `wait_until(|| !self.queue.lock().is_empty())` in `ranked_lock!` lost
the negation, and the kernel hung at boot right after the root mount with the
serial log simply stopping — the FS mailbox thread waiting on an inverted
predicate. The symptom looks like a deadlock in whatever ran last, not like a
typo. Re-read predicates after a macro rewrite.

## Ctrl+C kills the foreground job, and always did

Verified in the VM on 2026-08-10: `sleep 30` and a stdin-blocked `cat` both die
on Ctrl+C with the prompt returning, and Ctrl+C at an idle prompt leaves the
shell alive. `ideas.txt` claimed the kill delivery behind `LineAction::Interrupt`
was the one missing piece; it was already there (`PtyNotifications::kill_pid` ->
`flush()` -> `kill_process`) and the entry had gone stale.

**What keeps the shell alive is not the foreground bookkeeping.** `sys_spawn`
registers any child whose fd 0 is a PTY slave as `foreground_pid`, including the
session shell the terminal spawns, so at an idle prompt Ctrl+C really is
delivered to the shell. `edos-sh` sets SIGINT to SIG_IGN at startup and
`kill_process_with_signal` returns early on SIG_IGN. A negative-control kernel
that registers the shell unconditionally still leaves it alive, which is how
that was established — a plausible-looking "the shell would be killed" fix was
built, refuted by the control, and reverted.

## Ctrl-D ends a stdin read now

`Pty::slave_read` returned an empty `Vec` for two different things — Ctrl-D
(`eof_pending` consumed) and "no data yet" — so the caller could not tell them
apart and parked in both cases. A program reading stdin (`wc`, `sort`, `cat`
with no args) therefore hung with no way out unless the master closed. It
returns `PtySlaveRead::{Data,Eof,WouldBlock}` now, and `sys_read` breaks with 0
on `Eof`, which is how POSIX spells EOF.

Verified against a negative control, because the first end-to-end test was wrong
in a way worth recording: with `Eof` folded back into no-data, `wc -l` hangs and
the next command is swallowed as stdin; with the fix it prints the count and
returns to the prompt.

**The userspace chain was never broken, and a too-narrow grep said otherwise.**
`grep ctrl programs/edos-terminal/src/` finds nothing, which looks like "the
terminal has no ctrl handling". The handling is one layer down:
`edos_lib::keymap::map_keycode` maps ctrl+a..z to 0x01..0x1a and the terminal
*widget* in `edos_render` tracks the modifier. Grep the widget and the keymap,
not just the program.

**And `scripts/edos-vm key` splits combos on `+`, not `-`.** `key ctrl-d` sends
one bogus qcode and silently does nothing, which reads exactly like a missing
feature. `key ctrl+d` is correct, as the script's own help says.

## The syscall table is closed, and closing it found five data-loss bugs

`doc/AUDIT.md` §3 listed eight missing interfaces; all eight now exist and the
table is down to `setuid`, which is rejected there. 111 syscalls, each with an
`edos_lib` wrapper and a case in `programs/iotest` — **`iotest /var` is the
regression suite for the whole set, and it runs 20/20.**

The syscalls are not the interesting part. Writing them found five bugs that
predate them, every one a silent data corruption:

- **`VfsInode` identity was keyed by the dentry cache**, so any invalidation
  (truncate, rename, create, or the LRU at 256 entries) forked one file into two
  inodes with independent page caches. A dirty page stayed on the first and read
  back as zeros through the second, then landed on disk over newer data. Inodes
  are keyed `(mount_id, ino)` through `fs/icache.rs` now.
- **Every EFS timestamp was 93 days late** — the shared days-from-civil helper
  used `(153*m+8)/5` instead of `(153*(m-3)+2)/5`.
- **memfs kept two sizes**, so every short `/tmp` file reported and read back
  padded to its last 4 KiB page.
- **An EFS hole read as `Corrupted`** rather than zeros, and growing an inline
  inode past the 176-byte inline area panicked the kernel.
- **No filesystem checked whether a name was free**, so `mkdir`/`create`/
  `symlink` over an existing entry added a *second* directory entry with the
  same name.

The lesson worth carrying: each was found by writing the syscall that exercised
the layer, not by reading the layer.

## One defect class, found in three drivers

**A pooled DMA buffer is not zeroed on reuse, and every parser read a fixed size
without asking how many bytes arrived.** A short transfer therefore returned the
previous owner's bytes as device identity or as sector data. Fixed in xHCI
descriptors (`7591982`) and USB mass storage (`41e2c41`, where `block_size == 0`
also faulted the CPU and an oversized one made `read_sectors` loop forever).

**AHCI ATAPI has the same defect and is still open.** `execute_atapi_command`
drops the count the command header's `prdbc` already carries. It is verifiable
today with no new QEMU option: `-cdrom` on q35 lands on the ICH9 AHCI
controller, so the guest logs `Found ATAPI device on port 2` /
`Model: QEMU QEMU DVD-ROM` on every boot.

If you add a driver that reads out of `DmaPool`, this is the first thing to
check. `allocate_sized` does not zero, and documents why: it serves AHCI
per-command buffers up to 2 MiB, so a memset per pop is a storage regression.

## The shell was rebuilt, and the kernel gave two things back to userspace

The GUI now has proportional type (Lato for chrome, JetBrains Mono for the
grid, from `/share/fonts` via `fontdue`), a panel with launcher/tasks/status
regions and icons, an applications menu with working power controls, minimize
and maximize, and a desktop right-click menu. `programs/wintest` is the
reference for the widget toolkit and now models a disabled state and aligned
columns.

Two moves that matter beyond the pixels:

- **The kernel no longer knows what a title bar is.** It routes pointer events
  into client space, so it needs the offset -- but each window now carries the
  frame *its manager gave it*, through `property::FRAME`. There is no global
  decoration constant in the kernel, and different windows can be framed
  differently, which is what a menu needs.
- **`FLAG_DOCK` split into `FLAG_UNDECORATED` and `FLAG_NO_FOCUS`.** They were
  one flag, and a menu needs the first without the second: it has no title bar,
  and it must take focus because losing focus is how it closes.

- **Managing another process's window needs a privilege now.** It was ungated:
  any process could move, resize, minimize or post a close event to any window.
  Init holds the privilege by being the process the kernel starts, and grants it
  per spawn to the compositor and the panel (`kernel/src/window/shell.rs`,
  `SYS_WINDOW_GRANT_SHELL` 234). Two things fell out of writing it: the
  privilege has to follow a process's *threads*, because `pid` here is a
  thread's own id and there is no thread-group id, so a grant is propagated at
  `sys_clone`; and the shell table must be ranked *outside* the window registry
  and settled before it is taken, which the lock-order tracker caught on the
  first boot.

Traps this round produced, both of which cost a build cycle:

- **`WidgetContainer` wraps every widget to assign it an id and forwards each
  trait method by hand.** A method added to `Widget` with a default body is
  inherited by the wrapper and never reaches the real widget. It compiles, it
  looks right, and it silently does nothing.
- **A window created this frame is not in the window list the caller already
  fetched.** The panel's menu closed itself instantly because its absence from
  a stale list read as "destroyed".

## The shell's loose ends, closed

Four things the rebuild left open, and what each turned out to need.

**Windows are addressable by name.** `/proc/windows` publishes the kernel
registry, and the compositor copies that file into the kernel log on
`Ctrl+Alt+W`; the serial console is the only channel out of a headless guest, so
that keystroke is how the geometry reaches the host. `scripts/edos-vm windows`
and `focus <title>` are the host side.

Two details that are the whole difference between this working and looking like
it works:

- **The reported origin is the *outer* one and the reported size is the
  *client* one**, with the frame as a separate column, because that is what the
  kernel routes pointer events by. Clicking `x + w/2, y + h/2` lands in the
  title bar of a tall window and on the desktop below a short one.
- **Clicking a window's centre focuses whatever is on top of it.** `focus`
  subtracts every higher-z window's rect from the target's client area and
  clicks a point that survives, which is what raises a partly covered window;
  the first version clicked the centre and confidently focused the wrong window
  while reporting the right name. A fully covered window is reported, not
  guessed at.

**Wallpapers.** `edos_render::image` decodes 24- and 32-bit uncompressed BMP and
scales to cover; the compositor cycles the three generated lit grounds and every
readable `.bmp` in `/share/wallpapers` through the one desktop-menu entry. The
shipped image is generated by `scripts/mkwallpaper.py` at build time, since this
repo holds no binaries, and the make rule depends on the script so an unchanged
wallpaper keeps its timestamp — the disk-image manifest is timestamp-based, so
regenerating it every build would rebuild both images every build.

**The status area does something.** Volume drives the HDA output amps through
two new `/dev/dsp` ioctls; the gain scale comes from the codec's own Output
Amplifier Capabilities rather than a hardcoded `0x7F` (QEMU's reports 74 steps),
and zero mutes rather than attenuating to the quietest step. Network reports
link, address, gateway, resolver and MAC from a new `/proc/net`. That file
exists because `SYS_NETINFO` renders the same state *for a terminal*, ANSI
colour codes and all, and a UI parsing that would be reading a display format.

**`std` reaches the whole syscall table** (`edos_rt` 0.0.42, fork pin bumped):
symlinks, file times, `is_symlink`, vectored I/O, `nanosleep`, a `ReadDir` that
streams through `getdents` a chunk at a time instead of demanding a buffer for
the whole directory, `access` behind `try_exists`, and `openat` so an open no
longer allocates a `CString`.

One of the nineteen was not a wrapper. `File::set_times` needs to stamp a file
the caller holds *open*, and `SYS_UTIMENSAT` took a path; a `File` has only a
descriptor. The kernel grew the POSIX form — a null path means the file `dirfd`
names — which is `futimens`, covered by `iotest` test 9.

## Fixed: a new window was black until its client painted

Reported from a VNC session, and invisible to a screenshot taken a moment
later. A window was created **mapped** (`WindowInfo::new` set `visible: true`)
and `Window::new` immediately pointed the compositor at buffer 0 — a buffer
nobody had drawn into. Everything between `window_create` and the client's
first frame — allocating the second buffer, the title, the flags, the client's
own pre-render — was therefore composited as a black rectangle inside real
decorations.

Both halves had to go: a window is created unmapped and its client maps it with
`show()`, and no buffer is published until the first `swap_buffers`, which is
the only call that means "this is what I look like". A window with no buffer
composites as its own themed ground, so a client that maps before painting
costs a frame of empty window rather than a black hole.

`Window::resize` still publishes an unpainted buffer, deliberately: the old
pair is freed immediately after, so the alternative is leaving the compositor
holding a freed shm id.

## The USB HID driver reads report descriptors now

It bound a device on `bInterfaceClass == HID && bInterfaceProtocol == 1|2` and
then decoded one fixed layout, so it understood exactly two devices: a boot
keyboard and a boot mouse. Those protocol codes only mean anything on an
interface that declares the *boot* subclass, so `usb-tablet` — which declares
none — enumerated and was dropped, and the guest had no absolute pointer. Under
VNC that shows up as the host pointer drifting away from the guest cursor and
walking out of the window, which is a symptom two layers above the cause.

`drivers/usb/hid/report.rs` parses the item stream into a field map: bit
offset, width, signedness, usage, and whether the value is a position or a
displacement. That last flag is the whole difference between a mouse and a
tablet and it is stated by the Input item; nothing about a byte layout implies
it. A pointer is now bound because its descriptor says it has X and Y.

Things worth knowing if you touch it:

- **The boot decoder is still there, as the fallback** for a descriptor that
  will not parse. A device the driver used to handle must not be lost to a
  parser bug.
- **`SET_PROTOCOL` is only sent when the fixed layout is what will be decoded**,
  and only to an interface that declares the boot subclass. Asking a tablet for
  boot protocol stalls, and asking a mouse for it after reading its report
  descriptor would replace the layout that was just parsed.
- **The report length comes from the endpoint descriptor**, not from the four
  bytes the boot layout happens to use: a tablet reports six.
- `parse_pointer` only reads inside the collection that declares itself a
  pointer or a mouse. A keyboard descriptor can carry an X/Y pair in a vendor
  collection, and taking it would make the keyboard the pointer.
- The sched-test suite parses both descriptors QEMU emits and checks the
  decoded offsets, values, scaling and the absolute flag, plus that a keyboard
  and a truncated descriptor are both refused. 49 → 50 tests.

**A trap that cost a debug cycle, and it was in the host script.** QMP serves
one client at a time. `pointer_is_absolute()` opened its own connection while
the caller already held one, so it timed out, reported "not absolute", and the
script silently fell back to relative motion — which QEMU *does* apply to a
tablet, so the pointer still moved and only the clicks went missing. It takes
the caller's connection now.

## The cursor moved to its own plane

Reading report descriptors gave the guest an absolute pointer, which is what
the hardware cursor had been waiting for: `hw_cursor` in the window manager was
hard-coded `false` with a comment saying so. With it on, the compositor stops
painting the pointer into the framebuffer, so moving the mouse damages nothing
and costs one small message; a remote viewer is handed the image and draws it
at its own pointer speed. That is most of what "the mouse is not smooth" over
VNC was.

The cursor texture already had zero alpha where it is transparent, which is
what both the software blit and the cursor plane want, so there is one cursor
image rather than two. A shape change is an upload rather than a different
texture at composite time, and the flag falls back to the software cursor if
the display has no cursor plane to take it.

**`screendump` does not capture the cursor plane**, so screenshots no longer
contain a pointer. That is worth knowing before it is read as a pointer that
failed to move; it also means a screenshot is no longer a way to check where
the pointer is.

## What a frame costs, measured

Asked whether dragging a window was slow, and whether the hardware cursor was
covering for a slow compositor. It is not. `FrameStats` in the window manager
times the composite and the transfer and reports only when frames miss their
budget, so it is silent on a healthy machine.

Dragging a 640x480 window across a 1920x1080 screen for five seconds, KVM,
four cores:

| | |
|---|---|
| frames per second | 77, against a 74Hz target |
| composite | 1.56 ms average, 2.4-4.9 ms worst |
| transfer (`flip_rect`) | 0.4 ms average |
| frames over the 13 ms budget | **0** |

So the guest composites a drag in about 2 ms of a 13 ms budget and misses
nothing. What a VNC viewer shows is the remote-framebuffer limit: a moving
640x480 window damages its old and new rectangles, some 2.4 MB of pixels per
frame, and that has to be encoded and shipped. The cursor became smooth
because on its own plane it ships *no pixels at all*.

Then the same counter was asked what the *display* is being handed, because a
guest that hits its frame rate can still be producing more than a remote
viewer can carry. Dragging that window:

**~250 MB/s of raw pixels**, about 3 MB per frame at 77 frames a second.

That is the whole story of "dragging is not smooth over VNC/SPICE". A moving
window's old and new rectangles both change, so the damage is roughly the
window's area every frame, and a remote protocol has to compress and ship all
of it. Gigabit ethernet carries 125 MB/s. The viewer is oversubscribed two to
five times over, so it applies updates partially -- which is what reads as
tearing, and it shows up first on a title bar because that is the crispest
edge on screen. The guest is presenting whole frames: `transfer_and_flush`
polls both commands to completion before returning, so nothing is being drawn
into while the host reads it.

This is also what SPICE's `streaming-video=filter` is for: it re-encodes a
fast-changing rectangle as lossy video so it *fits*. Turning it off buys
sharpness and spends smoothness. There is no setting that buys both, and no
change inside the guest that makes a moving window stop being megabytes.

Two things the numbers say that are worth keeping:

- **The first frames are enormous** — one report at boot averages 240 ms with a
  2.16 s worst case, while fonts load and the shm buffers fault in. It is the
  slow first paint at boot, not a steady-state problem.
- **Cost scales with the screen, not with what changed.** The compositor
  rewrites all 1920x1080 pixels every frame and only limits the *transfer* to
  the dirty rectangle. 1.5 ms says that is affordable today; it is where to
  look first if it stops being.

## A filesystem cannot resolve a symbolic link, and now does not try

`iotest /tmp` stopped at test 10 with "read through link: entity not found"
while the identical `iotest /var` passed. The VFS hands each filesystem a
*mount-relative* path and each filesystem resolved link targets from its own
root, so a link at `/tmp/link` naming `/tmp/target` made memfs look for
`tmp/target` under the memfs root, which has no `tmp`. EFS only worked because
it is mounted at `/`, where mount-relative and absolute coincide.

Chasing the fix turned up two more of the same shape, which is why the answer
is broader than the symptom. A relative target can walk *out* of its mount
(`/tmp/l -> ../var/x`), and the filesystem clamps the `..` at its own root
instead. And a target that stays put can still cross into something mounted
*deeper*, which the filesystem also cannot see. There is no rule by which a
filesystem gets any of these right, because the mount table is not its to
read.

So a filesystem no longer resolves a link target at all. Its walk stops at the
first link it is asked to follow and reports `Error::LinkEscape`; the VFS asks
where the link pointed (`FileSystem::link_escape`, answering in the only terms
a filesystem has: an absolute target, or a relative one plus how many levels
above the mount point it started), turns that into an absolute path, and
restarts resolution from the VFS root. The hop cap lives in the VFS now, so it
counts hops across mounts rather than per filesystem.

Two consequences worth knowing:

- **Escalation is error-driven, not a pre-pass.** `fs::api::with_links` runs
  the operation and only redirects when it comes back `LinkEscape`, so a path
  with no symbolic links costs exactly one walk, as before. Probing each prefix
  with `read_link` would have been the obvious shape and is O(N) walks per
  lookup.
- **The follow/nofollow distinction had to move up.** It used to live inside
  each filesystem's walk. `LinkMode` now carries it from the API layer, because
  the redirect has to be computed the same way the operation walks: `unlink`,
  `readlink`, `symlink` and `rename` leave the final component alone, and
  everything else follows it. `rename` is the one operation holding two paths,
  so a retry could not say which side raised the error; it settles both with
  `resolve_links` before calling.

`open` caches the path on the descriptor, so it takes the resolved one:
`file_info_resolved` hands back the path it landed on, which differs from the
one asked for exactly when a link crossed a mount.

## The panel publishes where its own buttons are

`scripts/edos-vm launch` used to mirror `programs/edos-taskbar/src/{main,panel,
menu}.rs` by hand, because the panel's buttons are not windows and nothing in
`/proc/windows` accounts for them. Moving the layout silently misaimed every
scripted click: no compile error, no failing test.

The panel writes them out itself now, the same way the window manager copies
`/proc/windows` into the kernel log: `panel|` lines whenever the layout moves
(a window opening or closing, or the clock growing a digit), and `menu|` lines
as the applications menu opens. `klog_dump` in `edos_lib::io` is the shared
writer. `scripts/edos-vm` grows `panel` and `press <name>`, `launch` resolves
rows by label, and every layout constant is gone from the script.

The panel needs no request channel because it republishes on change, so the
last block in the log is current. The menu does: it exists only while open, so
`launch` notes where the log ends before clicking the launcher and only reads
what lands after that.

## What the symlink rework broke, and what that says about the test suite

A review of the finished diff found four regressions, all of the same shape and
none caught by `iotest` passing on both filesystems. Worth writing down, because
the shape is the lesson: **making a filesystem report an escape instead of
resolving it turns every caller that did not expect an error into a caller that
now fails.** The retry loop covers `fs::api`. Anything reaching the VFS by
another door does not.

- **Executing through a symbolic link stopped working.** `fs::api::resolve_inode`
  is how the ELF loader reaches a binary — `do_spawn`, `execve`, and the boot
  load of `bin/edos-init` — and it called `vfs::resolve` directly, outside the
  loop. `ln -s /bin/ls /bin/ll; ll` failed with ENOEXEC while `cat /bin/ll`
  worked, because the shebang probe goes through `read_bytes`, which retries.
  A wrong errno on a path that demonstrably exists is the tell.
- **`rename` and `rmdir` on EFS returned ELOOP and EIO.** Both resolved their
  target with the *follow* variant while `fs::api` asked for nofollow. Two
  pre-existing bugs fell out of fixing that: `mv link newname` used to make
  `newname` a second name for the link's *target*, and `rmdir symlink-to-dir`
  used to free the target directory. memfs had it right all along.
- **`open(O_CREAT)` through a symlinked directory left a permanently broken
  fd.** `create_file` retries and creates the file at the resolved path;
  `open` then cached the *unresolved* one, so every later read and write on
  that descriptor failed.

The general hazard the design carries: `link_escape` is asked with the *API's*
link mode, not the mode the filesystem operation actually walked with, and the
two agree only by convention. Every op-follows / api-nofollow pair produces
`Unsupported`, which surfaces as EIO. `rmdir` was the only live instance; a
filesystem operation added later that follows a final component the API says to
leave alone will do it again.

`iotest` now covers all four: exec through a link, create-write-read through a
linked directory, rename of a link, and `rmdir` refusing one. Plus a two-link
cycle, which is the case that proves the new loop terminates rather than hangs.

## procfs answers for per-process memory

Writing a graphical process viewer turned up the gap: nothing anywhere said how
much memory a process was using. The closest was the VMA *count*, which says
how many mappings exist and nothing about their size.

`/proc/processes` has an RSS column now and `/proc/<tid>/status` a `VM Size` and
a `Resident` line. Virtual size is the sum of the VMA lengths and is free.
Resident is counted from the page tables when read, and that is the decision
worth recording: a page enters a user address space from demand paging,
copy-on-write, `mmap`, shared memory and the loader, and leaves it from a dozen
`unmap` sites, so a counter maintained at each of them drifts the first time one
is missed — and a memory number that is quietly wrong is worse than no memory
number. The walk descends only into *present* entries, so the lazily faulted
mappings this kernel leans on cost one skipped entry rather than a probe per
page; probing each page of each VMA instead would have been O(virtual size),
which for a sparsely faulted mapping is most of the work for none of the answer.

The lock order is `vmas` (70) then `memory_manager` (80), in that order.

Holding the manager is not on its own enough to make the walk safe, which was
the other thing the review caught. The reaper calls `Thread::free` *before*
dropping the thread from the registry, and procfs snapshots the registry into
`Vec<Arc<Thread>>` first, so it can reach a `MemoryManager` whose PML4 frame is
already back in the allocator and possibly reused — and `mapper` is an
`OffsetPageTable<'static>` whose lifetime says nothing about that. Reading the
VMA count was safe because a Rust structure stays allocated; this is the first
reader that follows the raw frame pointer. `Thread::free` now calls
`release_page_tables()` under the mm lock before freeing the frame, and
`resident_bytes` returns 0 once that is set.

A first reading, `/bin/edos-wm`: 471 VMAs, 51100 KiB of address space, 42660 KiB
resident. `/bin/sh` 208 KiB and `/bin/ps` 60 KiB resident against ~300 KiB
binaries, which is demand paging visible in a number for the first time. Kernel
threads report `-` rather than a figure: they have no address space of their
own, and reporting the kernel's would be a lie that adds up.

## strace exists, and it is now the first thing to reach for

A program that failed silently used to leave nothing behind — this OS is driven
through screenshots and a serial log, so "it printed nothing" was the end of the
evidence. `strace` makes it the beginning. Full write-up in
[`strace.md`](strace.md); the parts worth knowing before reading code:

**It is not ptrace, and deliberately so.** `syscall_handler` is a single choke
point for the entire syscall surface, so tracing is an entry record before the
match, a return record after it, and a per-thread mark to decide whether to
write either. Nothing stops the target and nothing changes its scheduling.

**The mark is a generation, not a bool.** `Thread::traced` holds the trace
session it was marked under, and only counts while that equals the live
generation. Ending a session is therefore one increment rather than a walk of
the thread table, and a mark a dead tracer left behind cannot reactivate under
the next one. This matters more than it looks: a stale mark means a program
writing records into a ring nobody drains, forever.

**A tracer that dies releases the session**, because `thread_exit` calls into
the tracer for the `+++ exited +++` record anyway. Ctrl+C on `strace -p` leaves
nothing marked, which is verified behaviour and not an assumption.

**Records can be lost and the count is printed.** The target never blocks on the
tracer; a ring that fills drops and counts. A tool that silently omits calls is
worse than one that admits it.

**Three things the design bought that are worth keeping in mind:**

- `/proc/syscalls` publishes the kernel's own syscall table (number, name,
  argument kinds) and its errno names, so `strace` holds no duplicate that could
  drift the way `WindowListEntry` can. **Adding a syscall now means adding a row
  to `kernel/src/syscalls/table.rs`** or `strace` will print it as
  `syscall_NNN(0x…, 0x…)`.
- Buffer contents are captured on both sides: an input buffer on entry, an
  output buffer on return, sized by the return value. The output side finds its
  buffer through the arguments *copied at entry* and carried in a `TracedCall`,
  not through the registers as they stand on return — `sys_execve` rewrites the
  whole `SyscallContext`, so those can name a dead address space. An earlier
  draft relied on "the dispatcher only ever assigns to `ctx.rax`", which is
  false. That is what makes `write(1, "hi\n", 3)` and `read(3, "…", 4096) = 12`
  readable.
- A call still in flight prints `<unfinished ...>` and resumes later. `strace -T
  sleep 1` showing `<... nanosleep resumed> = 0 <1.000049>` is the answer to
  "the program is hung", not a guess about it.

## Signals became a real subsystem

`signal.rs` was 163 lines of pending bitmask and an ignore-or-die disposition.
Five things landed on top of it; `programs/sigtest` covers each and is the
thing to run before believing any change here.

**Suspension happens at a boundary, not where the signal lands.** A stop sets
`stop_requested` and wakes the target; the target parks itself in
`stop_if_signalled` at its next syscall return or its next tick out of ring 3.
That is the same boundary `killed` uses and for the same reason — it is where
the thread provably holds no guard. The consequence worth keeping: a process
suspended mid-`write` finishes the write first, so Ctrl+Z can never leave a
filesystem lock held for as long as a user leaves a job suspended.

**A handler runs by rewriting the syscall context.** Delivery builds a
`SigFrame` on the user stack — the whole interrupted `SyscallContext`, the old
blocked mask, and a magic word — then points `ctx.rip` at the handler with the
restorer as its return address. `sigreturn` reloads it. Three things that are
load-bearing rather than incidental:

- The frame is written **below the red zone** and 16-aligned so that `rsp+8` is
  16-aligned at handler entry, which is what the ABI actually requires.
- `sigreturn` **checks the magic and masks rflags** before loading. It restores
  `rip`, `rsp` and `rflags` wholesale from a user-writable address, so without
  those two checks it is a privilege escalation rather than a syscall.
- The saved `rax` is the interrupted syscall's **return value**, so a handler
  that runs between a call finishing and userspace seeing its result is
  invisible to the interrupted code. `sigtest`'s first case checks exactly that.

**Delivery is syscall-return only.** A thread spinning without entering the
kernel does not run a handler. Default actions still reach it from the tick, so
Ctrl+C kills such a process — it just cannot *catch* it. Extending this to the
tick path means building the same frame from a `CpuContext` instead of a
`SyscallContext`, which is the work that was deliberately not done.

**A handled signal must not also take its default action.** `kill_process_with_signal`
returns early when a user handler is installed, leaving the signal pending for
the handler path. Without that, a process asking to handle `SIGINT` gets killed
by it anyway. `deliver_unblocked_signals` puts handled signals back for the same
reason.

**`Pipe::write` used to ignore its readers entirely** — a write with nobody
reading buffered into the kernel heap forever, so `yes | head -1` was an
unbounded allocation rather than a broken pipe. It now returns `None`, which
the caller turns into `EPIPE` *and* a `SIGPIPE`.

## The listen side of TCP had never been run (2026-08-11)

`programs/tcpecho` is the first thing to call `listen`/`accept`, and it panicked
the kernel on the first call and then broke on the second connection. Both are
fixed; the mechanisms are worth keeping.

1. **`sys_listen` inverted the port-table order.** It held the socket lock
   (rank 260) and took the port table (250) under it. `handle_tcp` takes them
   the other way round on the receive path, so this was an AB/BA the rank
   tracker caught on the very first call: "tried to acquire 'sys_listen' (rank
   250) while holding 'sys_listen' (rank 260)". `sys_bind` had it right all
   along — validate under the socket lock, drop it, take the port table, then
   re-take the socket — and `sys_listen` now has the same shape.

2. **Closing an accepted socket unbound its listener.** The socket close path
   removes `(proto, local_port)` from the port table, and a socket returned by
   `accept` carries the *listener's* local port. So the first connection to end
   took the listening entry with it and the next SYN was answered with RST. The
   table maps a port to the socket that owns it, so the entry is now removed
   only when it names the socket being closed (`Arc::ptr_eq`).

Two things this exposed that are **still open** (both in engram): a segment
is dropped outright on an ARP cache miss and never retried, so the first inbound
connection after boot is lost; and the accept queue never drops an entry that
never reached Connected, so every half-open SYN permanently occupies a backlog
slot.

### Things that will bite you

- **The host reaches the guest on 127.0.0.1:2323**, forwarded to guest port 23
  (`--ssh-fwd` in `scripts/edos-vm`). That is the only way in: user-mode slirp
  has no route to the guest otherwise, so a server on any other port cannot be
  tested from the host.
- **The first connection after boot always fails**, because of the ARP drop
  above. Warm the cache with a throwaway connection before judging a server.

## Things that will bite you

- **`USER_VA_END` cannot be put in a `VirtAddr`.** It is `0x0000_8000_0000_0000`,
  the exclusive end of the user half, which is the *lowest non-canonical*
  address; `VirtAddr::new` panics on it with "virtual address must be sign
  extended in bits 48 to 64". Anything expressing a half-open range over the
  whole user half — `MemoryManager::resident_bytes_in` is the one that hit it —
  must carry raw `u64`s. The panic is not at boot: it fires the first time
  something reads `/proc/processes`, which on the desktop is the panel, so the
  session comes up and then dies a few seconds later.
- `make edos-x86_64.iso` re-invokes the kernel target **without** any
  `CARGO_FLAGS` you passed earlier, silently replacing an instrumented build
  with a plain one. Pass the flags to the ISO target itself.
- `cargo` does not notice that `std` changed. After rebuilding the toolchain,
  `cargo +edos clean` in `programs/` or you will keep linking the old one, and
  the build will cheerfully report success.
- `sg` is also the name of the `ast-grep` binary. Scripts that need the group
  tool must use `/usr/bin/sg`.
- **`alloctest` never exits, by design.** Its whole body is
  `loop { let v = vec![0u32; 256]; black_box(&v); drop(v); }`, an allocator
  soak with no termination condition. It is not a hang and not a bug. Anything
  that runs the stress binaries in sequence will sit there forever and silently
  buffer the rest of the input; run it last, or not at all.
- Symbol addresses move on every kernel rebuild, so resolve them from
  `kernel/kernel` at runtime rather than hard-coding them.
- **`make all` does not rebuild `sata-disk.img`**, and every `run` target
  attaches it and prefers it over the live-root ramdisk. A rebuilt program is
  invisible to the guest until `make sata-disk.img`, so a screenshot looks
  exactly as if the change did nothing.
- **`make sata-disk.img` used to fail while a VM was running** — `qemu-img`
  reported "Failed to get write lock" — and worse, when it ran underneath
  `make test` or `storage-check` the whole build died and read as a *test*
  failure. The rule stops the guest itself now, so this only bites a VM started
  outside `scripts/edos-vm`.
- **A kernel edit no longer rebuilds `sata-disk.img`.** It used to, every time:
  `live-root.img` depends on the phony `kernel` target, its recipe writes
  `filesystem/boot/kernel`, and that path was in the manifest whose timestamp
  decides the disk. `filesystem/boot` is excluded from the manifest now, so a
  kernel-only cycle skips the 5 GB create, the `efs-mkfs` populate and the
  qcow2 convert. The cost of the exclusion: the disk's own `/boot` can hold a
  stale kernel, which nothing reads because the run targets boot the ISO.
- **`make test` leaves the sched-test ISO in place.** A later `edos-vm start`
  boots the test kernel rather than the desktop; re-run `make all` before manual
  guest checks.
- **`cargo check` from the repo root uses the wrong toolchain.** The root
  `rust-toolchain.toml` says plain `nightly`, `kernel/` pins
  `nightly-2026-03-06`, and the `x86_64` crate does not build on current
  nightly. Use `make -C kernel check`.
- **`efs-fsck` aborts before its dir-tree pass on a dirty journal**, so a "0
  findings" line from a power-cut image proves nothing. Type `shutdown` in the
  guest rather than `edos-vm stop`: it syncs every filesystem and the resulting
  image checks clean with no `--repair` replay.
- **Nothing on screen is addressed by pixel any more.** Windows go by title
  (`edos-vm windows`, `edos-vm focus <title>`) from `/proc/windows`; the panel's
  controls go by name (`edos-vm panel`, `edos-vm press <name>`, `edos-vm launch
  <row>`) from what the panel itself publishes. No layout constant is left in
  `scripts/edos-vm`, so moving the panel no longer silently misaims every
  scripted click. A minimized window still has no geometry to click: `press
  <title>` hits its task button, which restores it.
- **The sched-test suite has a known flake with two signatures**: `ping-pong
  count mismatch: 499 != 500`, and a TIMEOUT with ping-pong-pong never
  reporting. It has been seen to fail **twice in a row** before passing, so a
  single clean re-run is weak evidence either way; weigh whether the changed
  code is reachable from the scheduler at all. Tracked in engram; it
  points at a lost or late wakeup, and it has never been chased.

## The shell can redirect any of the three standard descriptors now

`2>file`, `2>>file`, `2>&1`, `1>&2` and `&>file` work, on a plain command and on
each stage of a pipeline. Three things had to change, and the second was the one
that would have been diagnosed as "redirection is broken" forever:

1. `Redirects` is an ordered `Vec<RedirOp>` rather than three fields. Order is
   the whole semantics: `>f 2>&1` sends both streams to the file, `2>&1 >f`
   leaves the error stream on the terminal. `open_redirects` walks the list left
   to right into a three-slot table where each slot is either an opened
   descriptor or `Default(n)` — "whatever descriptor *n* would have been". A
   pipeline stage resolves that table against the pipe ends, which is what makes
   `ls / 2>&1 | wc -l` put the error stream into the pipe. `&>f` is `>f 2>&1`,
   never two opens of the same file, or the two descriptions would each start at
   offset 0 and overwrite each other.

2. **`split_chain` ate the `&` in `2>&1`.** It ran before any redirect parsing
   and treated an unquoted `&` at paren depth 0 as the background operator, so
   `ls / 2>&1 | wc -l` was split into `ls / 2>` and `1 | wc -l` — with the
   redirect code perfect, the command still made no sense. An `&` preceded by
   `>`/`<` or followed by `>` is part of a redirection, not a job-control
   operator.

3. **`>` never truncated.** The shell opened with `O_CREAT` only, so writing a
   short file over a long one left the tail behind. Adding `O_TRUNC` alone would
   have broken `> /dev/klog`, because devfs has no `truncate` and the trait
   default returns `IoError`: POSIX says `O_TRUNC` has no effect on anything but
   a regular file, so `open_resolved` in `kernel/src/syscalls/io.rs` now checks
   `FileKind` before truncating. Redirect opens also pass `O_WRONLY`; only
   `mmap` enforces the access mode today, but the descriptor should still say
   what it is for.

Only descriptors 0, 1 and 2 can be redirected. `SYS_SPAWN2` takes exactly three,
so `3>file` has nowhere to go; the shell says so rather than silently dropping
it.

### Things that will bite you

- **Pipeline exit status is still not tracked.** `run_segment` returns 0 for any
  pipeline regardless of what the last stage did, so `false | true` and
  `ls /nope | wc -l` both look successful to `&&`, `||` and `set -e`.

## The shell expands patterns now, and that broke `ls`

`programs/edos-sh/src/glob.rs` matches `*`, `?` and `[...]` (with `!`/`^`
negation and `a-z` ranges) one path component at a time, so `ls /bin/e*`,
`echo /bin/ec?o` and `for f in *.txt` all work. The rules that matter:

- **Expansion is per component, over `readdir`.** A word is split on `/`; a
  component with no metacharacter is appended literally, one with a
  metacharacter reads the directory built so far and keeps the names that
  match. That is what makes `/bin/*/x` cost one `readdir` per surviving prefix
  rather than a walk of the tree.
- **A pattern that matches nothing is passed through unchanged**, so
  `echo *.nomatch` prints `*.nomatch` — the shell convention, not an error.
- **Components after the last pattern are checked for existence** before the
  path is returned. `*/missing` was built by appending, not by reading a
  directory, so without the check it would be handed to the command as a path
  that does not exist. The check is skipped when the last component is itself a
  pattern, since those names came from `readdir` and are known to exist.
- **A leading `.` is only matched by a pattern that starts with a literal `.`**,
  so `*` does not pick up dotfiles and does not expand to `.` and `..`.
- **Quoted or backslash-escaped words are never patterns.** `parse_command`
  already flagged a word that was quoted anywhere; a backslash escape now sets
  the same flag, which also fixes `echo \>x` printing `>x` instead of
  redirecting. The flag is per word, so `a"b"*` is literal in its entirety —
  a deviation from POSIX, which tracks quoting per character.
- The command word itself is not expanded, only its arguments.

`extract_redirects` returns `Vec<(String, bool)>` rather than `Vec<String>` for
this: the quoted flag has to survive redirect extraction to reach expansion.

**`ls` could not take what globbing hands it.** It read `args[1]` and called
`read_dir` on it, so `ls /bin/e*` — now nine real paths — printed
`cannot access '/bin/echo': not a directory` and stopped. It takes any number of
operands now: non-directories are listed by name first, then each directory,
with a `path:` header when there is more than one operand. Any program that
takes "a path" is a candidate for the same defect now that a single word can
expand to many.

### Things that will bite you

- **`for f in *; do ...; done` on one line does not run.** Loops are a
  multi-line script construct; the interactive shell reads `for`, `do` and
  `done` as commands and reports them not found. This predates globbing and is
  unrelated to it, but it is the first thing you will try when testing a glob.

---

## `sed`, and the backslashes the shell was eating

`programs/sed` is a stream editor over its own backtracking regex engine
(`programs/sed/src/regex.rs`): POSIX BRE by default, ERE under `-E`, plus the
GNU extensions scripts actually use (`\+`, `\?`, `\|`, `\{m,n\}`, `\w`,
`\s`, `[[:class:]]`). Commands are `s`, `y`, `p`, `d`, `q`, `=`, `a`, `i`, `c`
and `{}` blocks; addresses are a line number, `$`, `/re/` (with `I`), a range of
either, and `!`. Options: `-n`, `-e`, `-f`, `-i[SUFFIX]`, `-E`/`-r`.

**The engine matches `&[char]`, not `&str`.** Capture offsets are character
indices, so a replacement splices out of the same `Vec<char>` without
re-scanning UTF-8 and a multi-byte character can never split a capture.

Two things in the matcher are not obvious and are load-bearing:

- **A repetition whose body matched empty must not recurse.** `m_rep` refuses a
  repetition that did not advance the position; without that, `\(a*\)*`
  never terminates.
- **An empty match abutting the previous match is not a new occurrence.**
  `substitute` tracks `prev_end` and skips an empty match starting exactly
  where the last one ended. Without it, `s/a*/-/g` on `baac` gives `-b--c-`
  instead of GNU's `-b-c-`.

**ROOT CAUSE FOUND WHILE TESTING IT: the shell was deleting backslashes inside
single quotes.** `parse_command` in `programs/edos-sh/src/command.rs` had one
backslash arm that escaped the next character unconditionally, "inside or
outside quotes". So `echo 'a\1b'` printed `a1b`, and every sed script written
the normal way — `sed 's/\(.*\) \(.*\)/\2 \1/'` — reached the program as
`s/(.*) (.*)/2 1/`, which is a BRE with literal parentheses: it matches
nothing, sed changes nothing, and the output is the input. This looked exactly
like a broken regex engine. Fixed to POSIX 2.2.2/2.2.3: inside single quotes a
backslash is literal, inside double quotes it escapes only `$`, `` ` ``, `"`
and `\`, and is otherwise literal.

### Things that will bite you

- **A sed script whose output equals its input is more likely a quoting bug
  than a regex bug.** Check what the program actually received before touching
  the matcher: `strace -o /tmp/t.txt sed '...'` prints the argv.
- **`"\$x"` inside double quotes still expands `$x`.** Variable expansion runs
  over the raw line before tokenizing, so it never sees the backslash. This is
  separate from the fix above and still open; `echo "d\$x"` prints `d"`.
- **There is no `printf` in the guest.** Build a fixture file with `echo` and
  `>>`, not with `printf '...\n'`.

---

## The shell has job control

`programs/edos-sh/src/jobs.rs` holds a `Job` with every stage's pid and the
process group they share; `JobStatus` gained `Stopped`. Ctrl+Z suspends a
foreground job, `jobs` lists it, `fg` and `bg` resume it, and one Ctrl+C
reaches every stage of a pipeline.

What makes it work, in the order it matters:

- **`spawn_pipeline` returns the pids and no longer waits.** The caller decides
  whether the job is foreground, which is the whole difference between a job
  and a blocking call. It also groups the stages as it spawns: the first stage
  leads, the rest `setpgid` into it.
- **The kernel already put a spawned child in a group of its own** and made it
  the terminal's foreground group, whenever its standard input is the pty
  slave (`sys_spawn`). That is what made Ctrl+C work before any of this. The
  consequence is that **the shell has to take the terminal back after every
  job, background ones included** — `reclaim_terminal()`. Miss that and the
  next Ctrl+C goes to a job nobody is looking at.
- **A segment is expanded and its redirections opened exactly once**, by
  `prepare_segment`, because expansion runs commands (`$(...)`). The result is
  either a builtin the shell runs itself or a list of pipeline stages, and the
  same value is what runs in the foreground or becomes a job. The previous
  background path re-parsed the segment inside a fork, which would have run
  every command substitution twice.
- **A background external job is no longer forked.** It is spawned directly, so
  the job is the pipeline itself and `fg` can hand it the terminal. Only a
  background *builtin* still forks, and that fork calls `setpgid(0, 0)` so it
  is not in the shell's group.

Two kernel changes were needed, both in the wait path:

- **`waitpid` with `WAIT_UNTRACED|WAIT_BLOCK` now blocks until the child exits
  *or* stops.** It only blocked on exit before, so a shell waiting on a
  foreground job would sleep through a Ctrl+Z. `stop_if_signalled` wakes the
  registered waiter the same way an exit does, and the wait loop re-registers
  each pass because waking consumes the registration. Without this the shell
  would have had to poll.
- **`SIGCONT` clears the target's `stopped` flag at delivery**, not when the
  target next runs. `fg` sends SIGCONT and immediately waits; the resumed
  process has not been scheduled yet, so the wait saw `stopped` still set,
  reported the job stopped again and put it straight back in the job list.
  Observed as `fg` printing `[2]+ Stopped cat` the instant it was typed.

### Things that will bite you

- **A stop takes effect at the target's next syscall boundary, so `sleep 30`
  ignores Ctrl+Z until it wakes up.** Nothing is wrong with the shell: the
  `[1]+ Stopped` line arrives 30 seconds later, and everything about it is
  correct then. `sys_sleep_ms` does not return early on a pending stop or kill.
  Test job control with `cat` — it is blocked in a pty read and stops at once.
- `fg` resumes a job and runs it, but `/proc` still shows the resumed process
  `Stopped` afterwards. Suspected in the same area as the SIGCONT fix above:
  the flag is cleared, but something re-sets it or the process re-enters the
  park. Reproduce with `cat`, Ctrl+Z, `fg`, then `ps` from another shell.

## The session has a timezone, and until now it had no environment at all

The kernel keeps time as UTC — it reads the RTC once at boot and answers
`clock_gettime` from a monotonic counter — and the panel clock formatted that
directly, so the desktop clock was wrong by the local offset for anyone not on
Greenwich. The fix is one offset, applied in one place:

- `edos_lib::time::utc_offset_seconds` reads `TZ` and
  `edos_lib::time::local_time` is UTC shifted by it. `ClockTime::from_unix_secs`
  is the shared constructor; `from_unix_nanos` stays UTC and now delegates to it.
  `ClockTime` also carries a `weekday`, which `date` needs and nothing computed
  before.
- **`TZ` holds a fixed ISO 8601 offset (`+02:00`, `-0530`, `+02`, `Z`), not a
  POSIX zone rule and not an IANA name.** There is no zone database and no DST,
  so a zone name parses as nothing and means UTC. This is deliberately *not*
  POSIX `TZ` semantics, where `UTC+2` means two hours **west**; ours is signed
  east, the way an ISO offset reads.
- `edos-init` sets `TZ` for the session, so a fresh boot has it. `export TZ=…`
  in a shell overrides it for everything that shell starts.
- `programs/date` prints it, `-u` for UTC, and a `+FORMAT` subset (`%Y %m %d %H
  %M %S %F %T %s %a %b %Z %n %t %%`). An unknown directive is passed through
  with its `%`, so a typo is visible instead of silently dropped.
- `cal` had its own year-by-year walk over the epoch to find today, in UTC. It
  is `edos_lib::time::local_time` now, and 40 lines shorter.

### The trap: nothing in the session had an environment

`TZ` set in `edos-init` reached nothing, because `edos-init` spawned its
services with `process::spawn`, which is `SYS_SPAWN` and passes **no envp at
all** — and `ChildProcess::spawn_shell` did the same for the shell under the
terminal. So every GUI process, and every shell in it, started with an empty
environment; `HOME`, `PATH` and `PWD` only ever appeared to work because every
reader has a hardcoded fallback. `SYS_SPAWN2` (path, argv, envp, three fds) had
existed since the shell learned to pass its environment on, but only the shell
used it.

`edos_lib::process::spawn_with_env` is `spawn` over `SYS_SPAWN2` with the
caller's environment, and init and `spawn_shell` both use it. The envp build is
`current_env_strings`, shared with `spawn_program_with_fds` rather than written
twice.

**If a new session-wide setting does not reach a program, check which spawn it
went through before looking anywhere else.** `SYS_SPAWN` is still there and
still silently drops the environment.

## `tar` exists, and it reads and writes what GNU tar does

`programs/tar` is ustar (POSIX.1-1988), the format every other implementation
falls back to: 512-byte header, 512-byte data blocks, two zero blocks to close.
`-c`, `-t` and `-x`, with `-v`, `-f` (`-` or absent means the standard stream)
and `-C`. Regular files, directories and symbolic links; the header module is
`programs/tar/src/header.rs` and is the only place that knows a field offset.

Interoperability is the whole point of picking ustar, so it was verified both
directions against GNU tar on the host, not just round-tripped against itself:
an archive our encoder wrote lists correctly under `tar tvf` with the right
sizes, modes, link targets and mtimes, and an archive GNU tar wrote decodes
here with the same. In the guest: create, list, extract and `diff` of the
result, `tar -cf - dir | tar -tf -` through a pipe, a selective `tar -tf a.tar
sub/dir`, and extracting a GNU-made archive off `/share`.

Three things the format demands that are easy to get subtly wrong:

- **The checksum covers the checksum field as eight spaces**, and is written as
  six octal digits, a NUL, then a space. Writing it as seven digits plus NUL, or
  as eight digits, is accepted by some readers and rejected by others.
- **Numeric fields are `width - 1` octal digits plus NUL**, zero padded, not
  space padded. The parser here accepts leading spaces and stops at the first
  non-digit, because implementations disagree about the terminator.
- **A path longer than 100 bytes splits into `prefix` and `name`** at a `/`,
  and the split must leave both halves inside their fields. Take the *longest*
  prefix that fits, so the remainder is as short as possible.

### Things that will bite you

- **`symlink_metadata` follows symlinks on this target**: `lstat` in the std
  fork (`library/std/src/sys/fs/edos.rs`) is literally `stat`. Nothing in
  userspace can ask "is this path a link" through `fs::Metadata`. What works is
  `fs::read_link(path)` succeeding, which is what `tar` uses to classify an
  entry. A `read_dir` entry's `file_type().is_symlink()` is also honest, since
  that comes from the directory listing, but it is only available while walking
  a directory.
- **Creating a symlink needs `edos_lib::io::symlink`.** `std::fs` has no
  portable symlink constructor and `std::os::edos` exposes only `ffi` and `io`,
  so there is no `std::os::unix::fs::symlink` to reach for.
- **`mkdir` used to read exactly one argument, and `mkdir -p` created a
  directory called `-p`.** It took `args[1]` and passed it straight to
  `create_dir`, so the flag became the operand, the call *succeeded*, and the
  script failed several commands later at the first write into the directory
  that was never made. It takes `-p` and any number of operands now. The shape
  of the bug is worth remembering: a program that indexes `args[1]` without
  parsing turns every flag into a plausible-looking success.
- **`scripts/edos-vm type` can lose the front of a long line.** A single `type`
  carrying four `;`-separated commands arrived with its first fifteen
  characters missing, so `mkdir -p /tmp/t/sub; …` ran as `/sub; …` and the
  failure looked like a shell parsing bug. Send one command per `type` and
  read the screenshot before trusting what ran.

## `top` exists, and it found `edos-procview` reading one column behind

`programs/top` is the thread table re-read on a timer in raw mode. The kernel
publishes only a *monotonic* `CPUms` per thread in `/proc/processes`, so a share
of the CPU is not something that can be read out of one sample: every percentage
in `top` is the growth of that counter across the interval just measured, and
the interval is timed with `Instant` rather than assumed to be the requested
delay, because a keystroke forces an early redraw and would otherwise divide by
the wrong number. The first frame, and the first frame in which a pid appears,
report zero.

The parse of `/proc/processes` moved out of `edos-procview` into
`edos_lib::procinfo` so both readers share one. Moving it is what exposed the
bug: **`PGID` was added to that table by the job-control work and the parser was
never taught about it**, so every field from `TYPE` rightward was one column
off. `edos-procview` had been rendering the pgid as the type, the type as the
state and the priority as the CPU, and looked entirely plausible doing it,
because each value it showed was a small integer or a short word in the right
shape for the column it landed in. The parser now reads every column the kernel
prints, in order, including the ones no caller wants: skipping a field by
position is exactly how a reader ends up behind the day a column is added in
the middle.

### Things that will bite you

- **A terminal line that fills the width exactly wraps on its own.** Clipping
  a row to `cols` and then writing `\r\n` costs two lines, not one, so a
  full-screen program that thinks it drew `rows` lines has actually drawn more
  and has scrolled its own header off the top. Clip to `cols - 1`. The symptom
  is a blank line between every long row and a missing header, which reads like
  a size-detection bug rather than an off-by-one.
- **There is no `/dev/null`.** `yes > /dev/null &` fails with `/dev/null:
  cannot open for writing`, which makes the usual way to spin a CPU for a
  measurement not work; use a program that writes nowhere, or redirect to a
  file under `/tmp`. Tracked in engram.
- **The desktop can take longer than ten seconds to reach a prompt.** Typing
  into a terminal that has not spawned its shell yet silently discards the
  line, and the screenshot then looks like the program did nothing. Take a shot
  and confirm the prompt is there before typing.

---

## `snake`, the first program with a clock nobody drives

`programs/snake` completes Phase 3 of the roadmap. Everything before it either
ran to completion or blocked on the user; this one has to redraw on a timer
*and* answer the keyboard, and that combination has exactly one correct shape:
each pass waits on `poll(stdin)` with whatever is left of the tick as its
timeout. Sleeping the tick and then reading drops every key pressed during the
sleep; reading without a timeout stops the clock.

Three details are the whole game and are easy to get wrong in a way that still
looks like it works:

- **The tick deadline lives outside the input loop.** A key redraws the frame
  so a turn looks instant, but it must not advance the snake (mashing keys
  would speed the game up) and must not push the deadline back (holding a
  direction down would stall it). Only the deadline passing moves the snake.
- **A reversal is judged against the direction actually travelled**, not
  against the last key. With one variable, pressing up-then-left inside a
  single tick turns the snake back into its own neck; with `dir` (applied) and
  `pending` (queued) it cannot.
- **The tail cell is vacated before the collision test.** Moving the head into
  the square the tail is leaving this tick is legal, and testing first reports
  a self-collision on every straight move once the snake is longer than one.

Food goes on the *n*-th free cell for a random *n* rather than on retried
random cells: the rejection loop is slowest exactly when the board is nearly
full, which is when the game matters. The occupancy grid that makes that cheap
is the same one the collision test reads.

### Things that will bite you

- **`\x1b[?25l` and `\x1b[?25h` are honoured** (DECTCEM). They used to do
  nothing: `parse_csi_params` in `edos_render/src/widgets/terminal.rs` ran
  `parse::<usize>()` over `?25`, got 0, and `l`/`h` had no arm, so the cursor
  stayed parked wherever a redraw had got to and read like a rendering bug in
  the program. The parser now recognises the DEC private-parameter prefix and
  skips it, and the mode drives a `cursor_enabled` flag that gates drawing
  separately from `cursor_visible`, which is the blink phase and must stay
  independent of it. `edos-sh` prints `\x1b[?25h` ahead of every prompt, because
  a full-screen program killed before it restores the mode would otherwise
  leave the cursor hidden for the rest of the session and there is no `reset`.

---

## `imgview`, and where a wallpaper and a picture stop agreeing

`programs/imgview` is the first ordinary GUI application in the tree: not the
compositor, not the panel, not a toolkit demo, just a window with a picture in
it. Most of it was already written — `edos_render::image` decodes the BMP and
resamples it, which is what made the program small — but the part that could not
be shared is the interesting one. A **wallpaper covers**: it scales until both
axes are filled, crops the overflow about the centre, and never letterboxes,
because a desktop with bars of dead colour at the edges is not a ground. A
**viewer fits**: it scales until the whole picture is inside the frame, lets the
surrounding surface show, and does not enlarge past 100%, because magnifying by
default hides what the file actually contains. Those are opposite policies over
the same arithmetic, so `scaled_to_cover` and `scaled_to_fit` now sit beside
each other over one bilinear `resample_at`, which takes a per-axis step and a
source origin; cover passes the smaller step twice with a centred origin, fit
passes both steps with the origin at zero.

The letterbox itself belongs to the caller, not to the scaler: only the program
drawing knows what colour the surface behind the picture is. `imgview` fills
with the theme background, so an image of another aspect ratio sits on the same
ground as the rest of the shell.

### Things that will bite you

- **The kernel never sends a `Character` window event.** `WindowEvent::character`
  exists in `kernel/src/window/input.rs` and nothing constructs it:
  `handle_keyboard_event` routes `KeyPress`/`KeyRelease` carrying a raw
  scancode, and that is all a client ever sees. The kernel has no keyboard
  layout and should not grow one, so a program that wants letters maps them
  itself with `edos_lib::keymap::{update_modifiers, map_keycode}` — which is
  exactly what the widget container and the terminal already do. A viewer
  written against `event.character()` compiles, runs, draws correctly and
  silently ignores every key; the first screenshot after pressing one looks
  identical to the one before it, which reads like a redraw bug rather than an
  event that was never delivered.

## Fixed: closing a window left the keyboard pointed at a deaf one

Quitting a GUI program cost a reboot: every keystroke after it was dropped, and
clicking the terminal did not help even though its title bar and task button
both painted focused. Three separate pieces each behaved correctly and the
composition lost the keyboard.

`WindowRegistry::destroy_window` did move focus — it picked `topmost_focusable`
and stored it — so `/proc/windows`, the compositor's decorations and the panel
all named the terminal focused, which is why the screen looked right. What it
did not do is tell the winner. The client's own belief about focus comes from
`FocusGained`/`FocusLost` events alone, and `edos_render`'s terminal widget
drops `on_key` outright when it thinks it is unfocused. The terminal had been
told `FocusLost` when the viewer's window was created (`create_window` returns
the displaced holder for exactly that purpose), and nothing ever told it
otherwise.

Click-to-focus could not repair it either, and for a defensible reason:
`handle_mouse_event` compares the click target against `registry.focused_window()`
and sends nothing when they already agree. That is right — a click inside the
focused window must not restage focus — but it means the registry and the client
can never re-synchronise once they disagree. The registry has to be the one that
never lets them.

So a focus transition is only real when its event is delivered, and every
registry call that moves focus now returns the window that has to be told:
`create_window` already did, `set_minimized` and `release_dock_focus` already
did, and `destroy_window` and `destroy_windows_for_pid` now do too. The two
callers — `sys_window_destroy` and `window::cleanup_process_windows` — send
`focus_gained` after dropping the registry lock, the latter after the dead event
queues are removed. `destroy_window` no longer returns `bool`: both callers
establish existence under the same lock, so the flag was never read.

Both paths matter and they are different code: a program that closes its own
window goes through the syscall (`edos_render`'s `Drop for Window` calls
`window_destroy`), and one that is killed or panics goes through the process-exit
cleanup. Verified separately in the guest — `imgview` quit with `q`, and
`imgview` killed by a delayed `sh -c "sleep 8; kill 28" &` while it held focus —
with the terminal accepting a typed command afterwards **without a click** in
both cases.

### Things that will bite you

- **A window that renders focused is not a window that receives keys.** The two
  answers come from different places: decorations and the task button read the
  `focused` flag out of `sys_window_list`, while a client decides whether to
  act on a key from the last focus event it was handed. When they disagree the
  screen shows the registry's answer, so the symptom is a window that looks
  live and behaves dead. `[Term] FocusGained` in the serial log is the ground
  truth for what the client believes.
- **Killing a windowed program while it holds focus needs a delayed kill,** or
  the terminal you type it in takes focus first and the exit path under test is
  never exercised: `sh -c "sleep 8; kill <pid>" &`, then click the target
  window. Its pid is reachable without reading the covered terminal by sending
  `ps > /dev/klog` and reading `scripts/edos-vm log` on the host.

## `ln` exists, and `lstat` in the std fork is a lie

`ln -s` was the last thing standing between the shell and symbolic links, which
the VFS has resolved correctly for a long time. Three POSIX shapes: one target
into the working directory under its own basename, one target to a named link,
and several targets into a directory. `-f` replaces an existing destination but
refuses a directory, since replacing one with a link would discard its
contents. Hard links are refused outright, and that is a statement about the
kernel rather than about `ln`: there is no `link(2)`, and EFS inodes carry no
link count, so there is nothing to increment.

The interesting part is what verifying it turned up.

**`std::fs::symlink_metadata` follows symbolic links on this target.**
`library/std/src/sys/fs/edos.rs` defines `lstat` as `stat(p)` — literally the
same call — so `Metadata::is_symlink()` can never be true and every
`symlink_metadata` caller silently gets the target's type and size. `stat` had
had a dead `is_symlink()` branch since it was written for exactly this reason:
it reported a link to an 11-byte file as `regular file, 11 bytes`.

The fix that does not require the Rust fork is `readlink`, which resolves the
final component without following it: a non-negative return *is* the proof that
a path is a link, and it hands back the target in the same call. `stat` and
`ls` both classify that way now. The real fix is in the fork — an
`AT_SYMLINK_NOFOLLOW` stat path plumbed into `lstat` — and it is written down
in engram.

`getdents` is not affected: it reports `file_type == 2` for a link, so
`DirEntry::file_type()` is correct and `ls` uses it for directory contents.
Only path-based lookups go through the broken `lstat`.

### Things that will bite you

- **Do not trust `Metadata::is_symlink()` on edos.** It is always false. Use
  `edos_lib::io::readlink`, or `DirEntry::file_type()` if the name came from a
  `read_dir`. Any code testing for a link through `symlink_metadata` is dead
  code that looks live.
- **`scripts/edos-vm type ... --enter` does not always deliver the Enter.**
  Several times in this session the line was typed and left sitting at the
  prompt until the next input arrived; a following `scripts/edos-vm key ret`
  fixes it. Screenshot after the `key ret`, not after the `type`, or the shot
  shows a command that has not run and reads exactly like a program that
  printed nothing.

## `watch`, and the escapes in other programs' output

`watch` re-runs a command every N seconds and paints the result over the
previous frame. Two things in it are worth keeping.

**Reading the child's pipe dry comes before waiting on it.** The command runs
with both its streams on one pipe; if the parent waited for exit first, any
command whose output exceeds the pipe buffer would block in `write` while the
parent blocked in `waitpid`, and the pair would sit there forever. Every
capture-the-output-of-a-child program in this tree has to do it in that order.

**A program's output is not a sequence of columns until its escape sequences
are separated out.** `ps` colours its state column, `ls` colours file types.
The first version of `watch` counted characters, and both column decisions it
makes came out wrong on exactly those lines: clipping to the terminal width cut
them about nine characters short, so `ps` names showed as a single letter, and
`-d` inserting a highlight in the middle of a colour sequence printed the rest
of that sequence as the literal text `7m4m`. It now splits a line into columns
that each carry the escapes preceding them, so escapes are zero-width for both
clipping and diffing. Tabs are expanded in the same pass, since the column a
tab lands on is only known there.

**`edos_render`'s terminal widget had no reverse video.** SGR 7 and 27 were
ignored, which is why `-d` looked like a no-op at first — and why `top`'s
inverse header and status bar had been rendering as plain text since `top` was
written. The pen now carries a `reverse` flag and swaps the pen colours as each
cell is written, so a highlight over coloured output keeps the colour. `watch`
ends a highlight with SGR 27 rather than SGR 0 for the same reason.

### Things that will bite you

- **Anything that clips, wraps or diffs another program's output must parse
  ANSI escapes.** Half this tree's CLI programs colour their output, so a
  character count is not a column count, and cutting a line at a character
  boundary can cut an escape sequence in half. `edos_lib::term` does it once:
  `cells()` splits a line into columns that each carry the escapes preceding
  them, `window()` takes a horizontal slice carrying the escapes scrolled past
  into the first visible column, and `render()` writes columns back out.
- **A frame ending in `\r\n` scrolls the screen.** Full-screen programs here
  write the last row without a line feed, or the terminal scrolls and takes the
  header with it. `top` and `watch` both do this; it is not obvious from either
  until the header starts creeping off the top.

## `less` (2026-08-11)

**The pager's keyboard is not always its stdin.** `dmesg | less` hands the pager
a pipe on fd 0, and this system has no `/dev/tty`: devfs registers `klog`, `fb`,
`kbd`, `tty0`, `random`, `mouse`, `dsp` and the block nodes, and `tty0` is the
kernel's own console, not the PTY the window's shell is on. What a pipeline does
leave pointing at that PTY is **stderr**, so `less` reads keys from fd 0 when
that is a terminal and from fd 2 otherwise, and puts *that* descriptor into raw
mode. Both `ioctl` and blocking `read` work on it; the PTY slave carries no
access mode that would stop either. With neither a terminal, it prints
everything and exits, which is what makes it safe in someone else's pipeline.

**Reading the text has to finish before the keyboard is touched.** The whole
input is read to EOF up front. That is not only simplicity: on a pipe the writer
is still running, and a pager that interleaved reading the pipe with reading
keys would be waiting on two descriptors that are the same terminal session.

**A search hit is a column range, not a byte range.** Matching runs over the
line with the escapes stripped out and the tabs already expanded — the `plain`
field alongside the cells — so a match index is directly the column to
reverse-video. The highlight is applied by pushing `\x1b[7m` into the escapes of
the first matched column and `\x1b[27m` into the one after the last, which means
it survives horizontal scrolling and clipping exactly the way the line's own
colours do, with no separate pass.

### Things that will bite you

- **`isatty(0)` is the wrong question for an interactive terminal program that
  can be at the end of a pipeline.** Ask it about the descriptor you intend to
  read keys from, and fall back to fd 2. A program that gives up when stdin is a
  pipe is unusable in exactly the case a pager exists for.
- **Forward search starts below the top line.** `/pat` reporting "pattern not
  found" for something visible three screens *up* is correct behaviour, not a
  bug; `?pat` is the other direction. This looks like a defect the first time.

## `pstree`, and the arguments the kernel never kept (2026-08-11)

`/proc/processes` has carried a PPID column since it existed, and every reader
printed it as a number. `programs/pstree` renders it as the forest it is, over
`edos_lib::procinfo::read_table` like `ps`, `top` and `edos-procview`.

**The tree the guest actually has is three deep and one of the levels is a
surprise.** `edos-init` supervises each child from its own thread, so what
appears under it is `edos-init-thread-20---edos-wm`, not `edos-wm`: the
supervisor threads are real rows in the table and the tree makes that visible
for the first time. Kernel threads have no parent in the table and come out as
roots, one per line, which is why `-u` exists.

**The layout rule is one line long.** A node's connector sits one column past
the end of its label, and its children start two columns past the connector;
that single rule produces both `a---b` and the aligned `a-+-b` / `` `-c ``.
Continuation lines carry a prefix string rather than a width, because an
ancestor's `|` has to keep being drawn down the left of everything under it —
a width alone cannot say which ancestors still have siblings to come.

**Compaction is restricted to subtrees that render on one line**, i.e. chains
where every node has at most one child, which is what `N*[sleep]` collapses.
A branching subtree has no unambiguous collapsed form, so it is left expanded
rather than guessed at.

### Things that will bite you

- **`/proc/<pid>/cmdline` now holds the arguments too**, so `sleep 60` and
  `sleep 1` are distinguishable in `ps`, `top` and `pstree`. `UserThread`
  carries an `Arc<String>` built by `load_process_image` from the argv it is
  already pushing onto the new stack; the kernel cannot read it back from the
  user stack later, because the process is free to overwrite it. It is per
  address space, so `execve` replaces it in `install_image` and `clone`/`fork`
  inherit the parent's. `/proc/processes` renders it as the trailing NAME
  column, which is why arguments containing spaces do not break the fixed
  columns anything parses. `pstree` had a `-a` flag for about ten minutes
  before the gap turned up and ships `-l` (whole spawn path) instead.

## `sntp`, and the clock the kernel could not be told (2026-08-11)

`kernel/src/timer.rs` reads the RTC exactly once, at one-second resolution, and
answers every later `clock_gettime` from that pin plus HPET ticks. On a fresh
boot that is around 1.4 s behind real time in the QEMU guest, measured against
`time.cloudflare.com`, and nothing existed to correct it: there was no way to
set the wall clock at all.

`programs/sntp` is the client (RFC 4330: one 48-byte packet, mode 3 out and
mode 4 back, offset `((T2-T1)+(T3-T4))/2` and delay `(T4-T1)-(T3-T2)`), and
`SYS_CLOCK_SETTIME` (281) is what lets it act on the answer.

**The step is an atomic offset, not a re-pin.** `WALL_CLOCK_OFFSET_NS` is added
in `wall_clock_nanos`, so the RTC reference point and the monotonic counter are
never touched — a step moves the wall clock and nothing that measures a
duration. Re-pinning would have meant making `WALL_CLOCK` mutable and taking a
lock on a path that every redraw calls.

**The reply is checked before it is believed.** Mode must be 4, stratum 0 is a
kiss-o'-death and stratum above 15 is unsynchronised, and the originate
timestamp must equal the transmit timestamp that was sent — that last one is
the anti-spoof check, and it is why the client's own T1 goes into the packet
rather than a zero.

**NTP seconds wrap in 2036,** so a timestamp below the Unix epoch delta is in
era 1 and gets `2^32` added rather than being read as a date in 1900.

### Things that will bite you

- **Do not pick a syscall number by grepping `^const SYS_`.** Several are
  declared `pub const`, and the `*at` family sits at 257–269 above what looks
  like the top of the range. 257 was taken by `SYS_OPENAT`; the dispatch arm
  compiled and the only sign was an `unreachable pattern` warning in the noise
  of the twelve pre-existing ones. Grep `^(pub )?const SYS_` and take the
  number above 280.
- **A UDP send to an unreachable address on the guest's own subnet fails
  immediately** ("send failed") rather than timing out — there is no ARP reply,
  so nothing is ever transmitted. `-t` only bounds a reply that never comes
  from a host that did answer ARP.

## `lsof`, and why `/proc/<tid>/fd` is a table and not a directory (2026-08-11)

"Which process still has this open" had no answer: the descriptor table lived
on `UserThreadInfo` and nothing published it. `/proc/<tid>/fd` does now, and
`programs/lsof` reads it.

**It is a text table, not Linux's directory of symbolic links.** Half the
descriptors in this system have no path — a pipe end, a PTY side and a socket
are all nameless — and the fields that identify them do not fit in a link
target. So a row is `FD TYPE MODE POS NAME`, and NAME is the rest of the line
because a socket's is several tokens:

```
0 pty rw 0 pts:[ffffc00024848810]
3 file r 657212 /share/fonts/Sans-Regular.ttf
4 pipe w 0 pipe:[ffffc0024848410]
5 socket rw 0 tcp:0.0.0.0:2400->*:* LISTEN
```

The bracketed number is the address of the shared object (`Arc::as_ptr`), which
is what makes the two ends of a pipe and the two sides of a PTY pairable across
processes — verified in the guest: `lsof | grep pipe` shows the writer and the
reader on the same `pipe:[…]`, and the terminal's `ptmx:[…]` matches the
`pts:[…]` of everything running under it.

**Two locks had to be released before two others in `render_fds`.** The table
handle is cloned out from under the thread-info `IrqSpinlock` before the table
itself is locked, because that spinlock runs with interrupts off and the table
is a `BlockingMutex` whose contended acquisition parks. Then the descriptors
are cloned out from under the table lock before they are rendered, because
describing a socket takes the socket lock (rank 260). A `FileDescriptor` clone
shares the underlying object without touching the pipe/PTY/socket open counts —
only `close` adjusts those — so cloning is safe here where `inc_refcount` would
not be.

Path-based procfs reads are safe to park in: `vfs::read` drops the inode guard
before calling `fs.read_bytes` when the inode is `None`, which is every
procfs file.

### Things that will bite you

- **The kernel's unit is the thread, so `lsof` reports a multi-threaded
  process once per thread**, with the same descriptors each time. That is what
  procfs holds; it is left visible rather than collapsed.

## `nc`, and the pipe hang-up that `poll` refused to report

`programs/nc` is both halves of a TCP connection: `nc host port` connects,
`nc -l port` binds, listens and accepts. One relay loop serves both. It polls
standard input and the socket together and copies whichever is ready, so a pipe
feeding one side and a peer answering on the other never block each other.
Flags are the ones that carry their weight: `-l`/`-p`/`-s`/`-k` for the listen
side, `-n` to refuse resolving a name, `-z` to connect and report without
transferring, `-w` for an idle timeout, `-q` for how long to wait after end of
input, `-v` for progress on stderr.

**Standard input is read with the raw `read` syscall, not `std::io::Stdin`.**
`poll` reports what the descriptor holds; a buffered reader that had already
drained it would make the loop wait on data it is holding.

**End of input half-closes rather than closing.** `edos_lib::net::shutdown`
wraps `SYS_SHUTDOWN` (247), which was implemented and reachable from no program
until now — `build_fin` in `kernel/src/net/tcp.rs` moves ESTABLISHED to
FIN_WAIT_1 and queues the FIN for retransmission, so the read side keeps working
afterwards. That is what makes `echo hi | nc host 7` send its line, let the peer
see end of input, and still print the answer. Closing instead would discard the
reply along with the connection.

### `poll` never reported a pipe whose writer had gone

The first run of `echo hi | nc 10.0.2.2 9099` sent its line and then hung
forever. `strace` was unambiguous:

```
read(0, "hi\n", 4096) = 3
write(5, "hi\n", 3) = 3
poll(0x428368, 2, -1) <unfinished ...>
```

`echo` had already exited, so the pipe had no writer left and a `read` would
have returned 0 at once — but `poll` slept. Two defects, both fixed:

1. **`PollState::matches` (`kernel/src/fs/mod.rs`) required the caller to ask
   for hang-up before it would report one.** POSIX makes POLLERR, POLLHUP and
   POLLNVAL output-only: they are reported whether or not `events` lists them,
   precisely so a reader waiting for data cannot wait forever on a descriptor
   whose peer has gone. `matches` now returns ready for error, hang-up and
   invalid unconditionally, and consults the interests only for readable and
   writable.
2. **`Pipe::poll_state` (`kernel/src/thread/pipe.rs`) set only `hangup` on a
   drained, writerless pipe, never `readable`.** A read there returns end of
   file immediately, which is readable in every other sense; both PTY sides
   already reported it that way. Now the pipe does too.

Either fix alone unhangs `nc`, and both are right independently: (1) is the
general rule and (2) is what makes the state honest. Anything that polls a
descriptor for readability and expects to notice end of input was affected —
which, before `nc`, was nothing, because every existing poll loop sits on a PTY.

`net::send_all` in `edos_lib` came out of the same run. A TCP write returns 0
when the send window is full rather than waiting, and both `nc` and `tcpecho`
had open-coded a retry loop that treated 0 as failure — silent data loss the
moment a peer reads slower than it is written. One helper now retries with a
millisecond pause; a peer that has gone away leaves ESTABLISHED, so the write
fails outright instead of spinning.

### Things that will bite you

- **`nc` has no UDP mode.** `-u` is not accepted. `edos_lib::net` has the
  datagram calls, but a datagram relay is a different loop (no connection, no
  end of input to propagate), and nothing needed it yet.

## `httpd`, and the two ways a listener stopped listening (2026-08-11)

`programs/httpd` serves a directory tree with one thread per accepted
connection. It is the first program in the tree to take more than two
connections in a row, and it stopped serving after the first one, then after
the eighth. Two separate defects, both in the kernel.

**The port table was keyed by port and released by anybody.** Closing a socket
removed `(proto, port)` from `PORT_TABLE` unconditionally. A socket returned by
`accept` carries its *listener's* local port, so the first accepted connection
to close unbound the listener, and every later SYN found no entry and was
answered with RST. This was found and fixed once before, for `tcpecho`, in
`syscalls/mod.rs::close_fd_refcount` — but the same sequence is written out
three times, and `syscalls/io.rs::sys_close` and
`thread/pipe.rs::close_descriptor` still had the unguarded remove. `tcpecho`
survived because it closes its listener between runs; `httpd` keeps one.

The rule now lives in one place: `port_key` and `unbind_port` in
`kernel/src/net/socket.rs`, the latter removing an entry only when the table
holds that exact `Arc` (`Arc::ptr_eq`). All three close paths call it, and all
three now read the key under the socket guard and release the entry after
dropping it — the receive path takes the port table before a socket, so the
other order is an AB/BA against it. Two of the three were taking the socket
with a bare `.lock()`, invisible to the rank tracker, which is why the
inversion had never been reported.

**A retransmitted SYN ate a second backlog slot.** `stack.rs` pushed a new
`Socket` onto `accept_queue` for every SYN, and `sys_accept` only removes
entries that reached `Connected`. A peer whose SYN-ACK was dropped retransmits
the same SYN from the same port; each copy took another slot, none of which
came back, so a backlog of 8 filled after a handful of connections and the
listener RST everything. The SYN path now drops the half-open entry left by the
same remote address and port before starting the handshake again, which is what
RFC 793 §3.4 asks for: a retransmitted SYN is one connection attempt, not
several. Measured after the fix: 12 sequential requests and 4 concurrent ones,
all 200, the 657212-byte font byte-identical each time.

### Things that will bite you

- **The first inbound connection after boot is still lost.** The guest has no
  ARP entry for the peer when the SYN arrives, `send_ip` returns "arp pending"
  and drops the SYN-ACK, and nothing retries it. The ARP reply lands ~40µs
  later, but the client's retransmitted SYN matches an existing connection in
  `tcp_connections` and so never reaches the listener path that would resend a
  SYN-ACK. Every guest test of a server therefore starts with one failed
  request; make it a warm-up rather than reading it as a bug in the program.
  The fix is a one-slot pending-transmit queue per ARP request (tracked in engram).
- **`httpd` answers one request per connection** (`Connection: close`), so it
  needs no idle timeout and keep-alive is not implemented. A client that opens
  a connection and sends nothing holds a thread until it goes away.

## `netstat`, and the socket list `/proc/<tid>/fd` cannot give (2026-08-11)

`/proc/sockets` is the connection table: every entry in `NetStack.tcp_connections`,
then every `PORT_TABLE` binding that has no connection of its own, as
`PROTO RECVQ SENDQ LOCAL FOREIGN STATE`. `RECVQ` is what has arrived and not
been read, `SENDQ` is `snd_nxt - snd_una`, what has been sent and not
acknowledged.

It is deliberately not derivable from `/proc/<tid>/fd`, which `lsof` reads. A
connection outlives the descriptor that made it: a `TIME_WAIT`, a `FIN_WAIT2`
or a stranded `SYN_RECV` belongs to no process at all, and those are exactly
the states worth looking at when a port cannot be bound again. The two files
answer different questions and both are needed.

The file is `/proc/sockets` rather than the `/proc/net/tcp` the roadmap
suggested, because `/proc/net` is already a file — the panel's network
indicator parses it — and procfs has no directories other than one per thread.
Turning `net` into a directory would have broken that reader for a cosmetic
gain.

A bound TCP socket that already has a `tcp_conn` is skipped, because the
connection table lists it with its sequence space; without that rule every
established connection appears twice.

Locking: both tables are snapshotted and released before any socket (rank 260)
or connection (270) is locked. Holding `NET_STACK` (240) or `PORT_TABLE` (250)
across them is legal by rank, but it parks the whole stack behind one `cat`.

`netstat` reads that file for `-a`/`-l`/`-t`/`-u`, and `/proc/net` for `-i` and
`-r`. There is no routing table in the kernel — an address, a prefix and a
gateway are the whole forwarding decision — so `-r` reconstructs the two routes
those imply rather than the kernel inventing a table to be asked for.

Both open TCP defects were visible on its first run, which is the argument for
having written it: the connection lost to the ARP-pending drop after boot sits
in `SYN_RECV` with one unacknowledged byte, and it is *still there* several
connections later, because the accept queue has no half-open timeout.

### Things that will bite you

- **The terminal is about 70 columns at its default window size**, not 80. A
  column-formatted table wider than that wraps every row and the output becomes
  unreadable in exactly the screenshot you take to verify it. `netstat`'s row
  is 67 characters wide for that reason. Count the format string before
  boarding a new program's table, or widen the window first.
- **A background job still owns the terminal's input.** `nc -l 23 &` relays
  standard input to the socket, so anything typed at the prompt afterwards goes
  to the peer rather than to the shell. Use a server that does not read standard
  input (`tcpecho -p 23 -q &`) when the point of the test is to keep typing.

## `fg` and the job that reported Stopped (2026-08-12)

The symptom on record — resume a stopped job and `ps` still says `Stopped` —
**does not reproduce**. Driven in the guest on two terminals: `sleep 300`,
Ctrl+Z (`[1]+ Stopped`), `fg`, then `ps` from the other terminal reports the
`/bin/sleep` thread `Sleeping`. `bg` and a bare `kill -TSTP` / `kill -CONT`
pair both show `/proc/<tid>/status` going `Stopped` → `Sleeping` as well.

The entry stays as a record rather than being deleted, because the symptom was
real and one path could still produce it. `SIGCONT` clears both
`stop_requested` and `stopped`, and the target clears `stopped` again on its
way out of the park in `stop_if_signalled` — but only the send path did that.
`deliver_unblocked_signals`, which acts on signals a widening `sigprocmask`
just unblocked, carried its own copy of the same match and its Continue arm
cleared `stop_requested` alone. A thread resumed through *that* door was
runnable while still reporting `Stopped` to `ps` and to an untraced `waitpid`,
which is exactly the report: a shell that resumes a job and polls it would put
it straight back in the job list. Both callers now go through
`apply_default_action`, so the two arms cannot drift again.

Ruled out along the way: `stop_if_signalled` itself (it stores `stopped=false`
unconditionally on the way out, and the loop added in `fc39bed` means it
actually reaches that store), the level-triggered `waitpid(WUNTRACED)`, and
`fg`'s `tcsetpgrp`/`pty_set_canonical` bracket.

### `kill` had two argument orders, and the wrong one killed the process

Reproducing any of this first cost a process: `kill 27 20` **terminated** pid 27
with `Done(143)`. The shell builtin took `kill [-SIGNAL] PID`, ignored every
operand past the first, and defaulted to SIGTERM; `/bin/kill` took the opposite
order, `kill PID [SIGNAL]`. Since the builtin shadows the binary, the form that
reads as "suspend 27" was parsed as "terminate 27". Both now take
`kill [-SIGNAL] PID...` over `edos_lib::process::signal_by_name` (names with or
without the `SIG` prefix, or a number), and the signal is only ever read from
the `-SIG` position, so `kill -TSTP 27` means what it says. Note the corollary:
every remaining operand is a PID, so `kill 27 20` now signals *both* 27 and 20
rather than dropping the 20 — POSIX, but still not "suspend 27". Reach for the
name form.

Verified in the guest: `kill -TSTP 29` then `grep State /proc/29/status` reports
`Stopped`, and `kill -CONT 29` reports `Sleeping`.

---

## The `-w-p` heap region is real, and the kernel was recording it faithfully (2026-08-12)

`pmap` on any process shows one anonymous region with no read bit:

```
/ $ pmap -x 27
27:   /bin/sleep 200
Address          End                 Kbytes    RSS Mode Mapping
0000000000400000 0000000000407000        28      8 r--p file:1:73+0
0000000000407000 0000000000413000        48     44 r-xp file:1:73+24576
0000000000413000 0000000000415000         8      8 rw-p file:1:73+69632
0000000000425000 0000000000435000        64      4 -w-p anon
00006ffff7df000  00006ffff7e0000         4      4 rw-p tls
00006ffff800000  0000700000000000     8192      4 rw-p stack
```

Confirmed, so the entry was not refutable — but every pointer on record for it
was wrong. There is no `brk`/`sbrk` syscall in this kernel; `heap_break` is only
a starting address for `next_mmap_addr`, and `syscalls/memory.rs` builds
`vma_prot` straight from what the caller passed. Both VMAs `thread.rs` creates
are `READ | WRITE`, and the loader maps the ELF `p_flags` one for one.

The `-w-` region is the userspace heap, and the request comes from the runtime:
`edos_rt`'s allocator called `mmap` with `PROT_WRITE` alone at both of its call
sites, for the 64 KiB pool chunk and for a large allocation past the 512 KiB
threshold. The kernel recorded exactly what it was asked for. That is what
Linux does too — x86 has no write-without-read page encoding, so a `PROT_WRITE`
mapping is readable in the PTE while `/proc/<pid>/maps` still prints `-w-p`.

Nothing in this kernel reads `VmaProt::READ` except the `pmap`/`maps`
rendering: `memory/fault.rs` checks only `WRITE` and `EXEC`, which is why a
heap that never asked to be readable has always worked. So the fix belongs at
the caller, and there is no kernel change. `edos_rt` 0.0.46 asks for
`PROT_READ | PROT_WRITE`.

Reaching userspace takes the whole publish loop, because a `0.0.z` requirement
is exact: publish the crate, bump the pin in `library/std/Cargo.toml` in the
fork, `cargo +nightly update -p edos_rt`, `./x install`, then `cargo +edos
clean` in `programs/` and rebuild. A skipped pin bump silently ships the old
crate and the region stays `-w-p`. The `cargo +edos clean` is not optional
either: the std rlib changes without its version string moving, so nothing in
`programs/` sees a reason to rebuild.

Verified in the guest after that loop: the same `pmap -x` on `/bin/sleep`
reports `rw-p anon`, and every region in the list renders a correct triple.

---

## FIXED: a zero-length transfer never looked at its descriptor (2026-08-12)

`syscallfuzz` listed 23 calls under "returned rather than failed" — a call that
answered success for a poison argument. Most of that list is the fuzzer being
honest about its own inputs: one in four pointer arguments is a *valid* scratch
buffer, so `getrandom`, `clock_gettime`, `netinfo`, `getdns` and friends
genuinely succeed, and `window_list` with `max == 0` returns the window count by
design. Those are not defects and the entries can be read past.

Six of them were one real defect with one shape. `read`, `write`, `pread`,
`pwrite`, `sendto` and `recvfrom` all opened with

```rust
if count == 0 { return 0; }
```

*before* resolving the descriptor, and `readv`/`writev` returned 0 for
`iovcnt == 0` without ever reaching `sys_read`. So `read(9999, p, 0)` on a
descriptor that was never open reported success. That matters because a
zero-length transfer is exactly how userspace probes an fd cheaply; answering 0
tells the caller a closed descriptor is live. Linux resolves the fd first
(`fdget` before `import_iovec`) and returns `EBADF`.

The fix is to validate first and short-circuit second, at each site:
`pread`/`pwrite`/`sendto`/`recvfrom` already resolve the descriptor and map its
error, so the `count == 0` return moved below that block and reuses it;
`read`/`write`/`readv`/`writev` go through `fd_is_open` in `syscalls/io.rs`.
`readv`/`writev` check unconditionally rather than only for `iovcnt == 0`, since
a vector whose buffers are all empty reaches no underlying call either.

Note that the null-pointer check has to stay *after* the length check:
`read(fd, NULL, 0)` is 0, not `EFAULT`.

`programs/iotest` test 20 covers it in both directions — the six calls fail on a
closed descriptor and still return 0 on an open one, so the test cannot be
satisfied by rejecting everything. After the fix the fuzzer's list was 17, all of
them the legitimate kind above — since the report learned to tell a poisoned case
from a plausible one (next section) it is 11.

---

## The fuzzer's "returned" list only counts poisoned cases (2026-08-12)

The list above needed a paragraph of prose to explain which of its rows were
findings, which is a report doing its reader's job badly. The reason is that not
every generated argument is poison, deliberately: one pointer in four is the
valid 4096-byte scratch buffer, and the length and integer sets lead with 0 and
1. Without those the kernel's own argument checks short-circuit every case and
the code past them is never reached — but a case built entirely from them asks
the syscall a question it *should* answer, so a success there is not evidence of
anything.

`arg_for` now returns the value together with whether it was poison, the scalar
sets carry a `plausible` prefix length (`Values` in
`programs/syscallfuzz/src/main.rs`), and a case is only reported when at least
one of its arguments was poison. Cases that succeeded with none are tallied as
`benign` on the call's row and in the summary, so coverage stays visible rather
than being silently dropped. Each report line now carries the arguments the call
was actually given, which is what makes a row readable without re-running it.
This also subsumes the old "a call with no arguments cannot be sent a bad one"
special case: `sched_yield` and `errno` simply report `benign=4`.

At `-n 4 -u 0`: 300 calls, `benign=37`, and 11 rows survive. Reading them by
argument, they fall into three classes and only the third is open work:

- **No failure return at all.** `isatty(0x1_0000_0000)` answers 0 because
  `sys_isatty` maps every descriptor that is not a stream or a PTY slave to 0.
- **A count query.** `list_dir`, `list_mounts`, `list_partitions` and
  `window_list` with `max == 0` return how many entries there are without
  touching the buffer, so a poison pointer alongside it is never dereferenced.
- **A length or maximum that is never bounded.** `getcwd(scratch+1, u64::MAX)`,
  `window_list(scratch, i64::MAX)`, `netinfo(scratch+1, i64::MAX)` and
  `getrandom(scratch, 0, u64::MAX)` all succeed: the write happens to fit, so
  nothing overruns *here*, but the absurd size was accepted rather than
  rejected. `open(path, u64::MAX)` is the same shape one level up — every flag
  bit set, `O_CREAT` included, is taken at face value. That is the next thing to
  fix, and it is the defect class the fuzzer's header paragraph names.

---

## FIXED: a declared length was never checked against the address space (2026-08-12)

The third class above is one defect with one fix. A length or maximum is the
caller's *claim* about a buffer it owns, and the kernel was only ever checking
the bytes it actually wrote — `try_copy_to_user` calls `access_ok` on the copy,
so a short answer into a huge declared buffer succeeds and the absurd size is
never contradicted. The claim itself has to be checked:

- `sys_getcwd` (`syscalls/io.rs`) and `sys_netinfo` (`syscalls/mod.rs`) call
  `access_ok(buf, len)` on the declared length before comparing it against what
  they have to say.
- `sys_window_list` (`syscalls/window.rs`) multiplies `max` by
  `size_of::<WindowListEntry>()` with `checked_mul` and checks that. It runs
  *before* the registry lock is taken, so the error return does not have to
  reach for the thread-info lock underneath the window registry (rank 280).
  The `max == 0` count query keeps its early return.

Two more had a validation the caller could skip by asking for nothing:

- `sys_getrandom` returned 0 for `count == 0` before looking at `flags`, so
  `getrandom(buf, 0, u64::MAX)` reported that flags this kernel does not
  implement were honoured. Validate, then short-circuit — the same ordering the
  zero-length transfer fix above landed on.
- `sys_futex_wake` (`syscalls/sync.rs`) never dereferences the word, it keys
  `FUTEX_REGISTRY` by the address, so nothing else would ever catch a kernel-half
  pointer the way `sys_futex_wait`'s `try_read_user` does. It now calls
  `access_ok` itself, before the `count == 0` return.

`open` is the same shape one level up, and the fix is to refuse rather than
ignore: `OPEN_FLAGS_SUPPORTED` is `0x3 | O_CREAT | O_TRUNC | O_APPEND`, which is
every flag this kernel implements, and anything outside it is `EINVAL`. So is
access mode 3, which removes the `_ => ReadWrite` fallthrough. This diverges from
Linux, where `open` ignores unknown bits and only `openat2` rejects them, and the
divergence is deliberate: dropping `O_EXCL` or `O_DIRECTORY` silently returns a
descriptor whose semantics are not the ones that were asked for. It is safe here
because userspace has exactly one source of open flags — `edos_rt`'s `OpenFlags`
(`READ_ONLY`/`WRITE_ONLY`/`READ_WRITE`/`CREATE`/`APPEND`/`TRUNCATE`), which std's
`OpenOptions` builds from, plus `edos-sh`'s `RedirMode::open_flags`. Both stay
inside the mask. Adding a flag to the kernel means adding its bit here too, or
every caller of it gets `EINVAL`.

Verified in the guest: `syscallfuzz -n 4 -u 0` PASS with the "poisoned yet
returned" list 11 → 6, `iotest /var` 20/20 (which is the real test of the open
mask, since it opens through std, `openat`, `O_CREAT`, `O_TRUNC` and `O_APPEND`),
and a desktop that composites — `edos-wm`, `edos-taskbar` and `edos-terminal` all
open device nodes and fonts on the way up.

The six rows that survive are the first two classes, and they are correct as they
stand: `isatty` has no failure return, and `list_dir`, `list_mounts`,
`list_partitions` and `clock_gettime` answer a question that a poison pointer
alongside a zero maximum does not make invalid. `futex_wake` stays on the list
with a *different* case than before — a valid address and a poison count, where
waking 1001 waiters on a word nobody waits on is 0 by definition. A row's
arguments are worth reading before assuming it is the same finding as last run.

## The socket address length was an output, not a value-result (2026-08-12)

`recvfrom`, `accept`, `getsockname` and `getpeername` each wrote a whole
`sockaddr_in` into the caller's buffer and then stored 16 into `addr_len`,
without ever reading what the caller had put there. A caller with room for less
than 16 bytes got the rest of its stack overwritten, and nothing told it the
address had been truncated. POSIX makes that argument value-result: capacity in,
real length out, copy bounded by the capacity.

All five sites now go through one `write_sockaddr_out` in
`kernel/src/syscalls/net.rs`. Nothing in the tree was passing a short capacity —
`edos_rt` and the std fork both initialise it to `size_of::<SockAddrIn>()` —
which is why this never showed as a crash, and also why the fix is safe: reading
the field would have broken any caller that left it uninitialised, and there is
none. Check that before extending the same shape to another call.

The receive flags were the same defect one layer up: `sys_recvfrom` took `flags`
as `_flags`, so `MSG_PEEK` consumed the datagram the caller asked to leave
queued. `MSG_PEEK`, `MSG_TRUNC` and `MSG_DONTWAIT` are implemented, and every
other bit is refused with EINVAL rather than ignored. `sys_sendto` accepts only
`MSG_DONTWAIT`, which is already its behaviour since a send here never blocks.

`programs/socktest` is the regression: it sends one real DNS query, then peeks
it three times, checks `MSG_TRUNC` reports the datagram rather than the buffer,
passes an 8-byte address capacity and checks the tail of its `sockaddr` is
untouched while the reported length is still 16, and finally consumes the
datagram the peeks left. It needs a reachable DNS server (QEMU user networking
answers on 10.0.2.3:53, the default) and is 6/6 in the guest.

DHCP also stopped hand-rolling its IPv4 header, which had been shipping `id=0`
since the identification field was fixed everywhere else. It cannot draw from
the stack's counter because it runs before the stack has an address, so it keeps
its own `AtomicU16`; the header is otherwise `ipv4::build`'s, which sets DF.

## Fragmented files: the instrument, and the hole it found (2026-08-12)

`fsbench fragprep /var` writes the same 16 MiB file `raprep` does, but in 256 KiB
steps alternating with a second file and `fsync`ing between each, so the two
files' blocks interleave on disk. EFS allocates at writeback, so buffered
appends to two files can still be flushed one file at a time and come out
contiguous; the `fsync` between steps is what forces the allocator to alternate.
Reboot and `fsbench ra /var` reads it cold, exactly as after `raprep`.

Both arms, one 16 MiB file read in 256 calls of 64 KiB on a cold boot:

| | contiguous | fragmented |
|---|---|---|
| read path | 287 MiB/s | 132 MiB/s |
| p50 per call | 203 us | 474 us |
| async prefetch windows | 245 | 5 |
| sync fallback windows | 3 | 243 |
| `extent_reads` / `runs` / `batches` | 7 / 10 / 7 | 247 / 672 / 247 |

So the queued-runs branch of `EfsDriver::read_via_extents` is real: a fragmented
read plans 2.7 physically contiguous runs on average and issues all of them as
one submit-then-reap round, where before it paid a device round trip each. The
contiguous file also plans more runs than reads, because a run longer than
`MAX_RUN_BLOCKS` (248 blocks, 992 KiB) is split whatever the layout.

The larger cost is not the runs, it is that the prefetch stops: 245 async
windows become 5, and 243 windows are declined and billed to the reader inside
its own `read`. That is where the 287 -> 132 MiB/s went, not into the extra
commands.

## An interleaved-append file reads zeros somewhere, and the size of "somewhere" was an artifact

**Open.** Repro: `fsbench fragprep /var`, reboot, `fsbench ra /var` — the pass
reports `VERIFY FAIL byte <n> of the file is 0x00, want <p>`. The contiguous arm
(`raprep`) verifies clean in the same build, so it takes the interleaved
append + `fsync` pattern to produce it.

**Ruled out: it is not a sub-block write, and the "one 512-byte sector" reading
of the two failing offsets was the instrument, not the bug.** `ra_check_edges`
compares only the **first and last 512 bytes of each 64 KiB call**, so a failing
offset says where the check looked. Both observed failures sit exactly on one of
those edges: 786432 is the head edge of call 12, and 1113600 is `1048576 +
65024`, the head of the tail edge of call 16. Nothing ever compared the 3584
bytes before it, so "the block's first 3584 bytes are right and its last 512 are
zeros" was never measured; neither was the `dd bs=512 skip=2175 count=1` probe,
which sampled the same 512 bytes and nothing else. `ra_check_edges` now walks the
whole chunk once an edge has failed and reports how many bytes differ and between
which file offsets, so the next failure states its own extent.

**Ruled out: the damage is not on the disk.** `scripts/fsbench-pattern-scan.py`
recognises any block of an fsbench pattern file inside a raw image from its first
16 bytes and compares all 4096, which answers the question without the guest's
read path in the way:

```bash
qemu-img convert -O raw sata-disk.img ~/.cache/tmp/sata.raw
scripts/fsbench-pattern-scan.py ~/.cache/tmp/sata.raw --tag 7 --size 16M
# 7672 pattern blocks in the image, 7672 byte-perfect, 0 damaged
```

That is the `fragprep` disk from the run that failed, and **no block anywhere on
it is partially written**: not one of the 7672 differs in a single byte. So
whatever the reader saw, there is no half-written block for it to have read.

What that leaves, and it is a different bug from the one written down before:

- The extent map names a **physical block that holds no file data** — a block
  `ensure_block_for_logical` zeroed and nothing later filled, or a mapping that
  points somewhere else entirely. `extent_holes` stays 0, so the range is
  mapped; being mapped says nothing about what is in it.
- Or the read plans the **wrong physical block** for part of a run, so the bytes
  come from a block that is not the file's. A whole-chunk report distinguishes
  these two immediately: a mapped-but-empty block reads as an aligned run of
  zeros, a mis-planned run reads as another file's pattern.

**The fixed instrument ran, and the answer is aligned zeros, not another file's
pattern.** Fresh `sata-disk.img`, `fsbench fragprep /var`, `sync`, reboot,
`fsbench ra /var`:

```
VERIFY FAIL  byte 1310720 of the file is 0x00, want 0x87; the chunk differs in
             16321 of 65536 bytes, 16321 of them zero, from byte 1310720 to
             byte 1363967 of the file
```

Every differing byte is zero, and 16321 is what a **16 KiB solid run of zeros**
looks like through this pattern: 1/256 of the pattern's own bytes are zero and
match, and 16384 - 16384/256 = 16320. The first bad byte is 1310720 = block 320,
4096-aligned and 64 KiB-aligned; the span to the last bad byte is 53248 bytes =
13 blocks, so four blocks' worth of zeros sit inside a 13-block window rather
than in one run. `extent_holes` is 0 for the pass, so nothing was planned as a
hole below EOF. That is the mapped-block-holding-no-file-data shape, and it is
per whole 4 KiB block.

**And the host scan now contradicts the guest, which is the next thing to
settle.** The scan grew a missing-blocks report — a block the guest reads as
zeros but whose pattern is on the disk was written and read back from the wrong
place; one that is nowhere in the image was never written:

```
7508 pattern blocks in the image, 7508 byte-perfect, 0 damaged
3584 of 4096 logical blocks have no copy anywhere in the image
```

The 512 blocks that *are* present are exactly logical 3584..4095, the file's
last 2 MiB, in one contiguous range. Blocks 0..3583 are absent at **every**
512-byte alignment, not just at 4096 (re-scanned with a 512-byte step: same 512
blocks, so this is not the scan's block alignment). Yet the same boot read
blocks 0..319 back with a correct pattern before failing at 320, on a cold boot,
and `ra_read` only opens and reads — it never writes the file. Both statements
cannot hold for one disk.

So the scan's unstated premise is what to check first: **that the guest's `/var`
is the image being scanned.** The boot log records `root=UUID=8765...` and
registers `/dev/sda`, `/dev/sdb` and `/dev/ram0`, but does not name which
partition won root, so nothing yet proves the reads and the scan looked at the
same device. Next: print the winning device in the mount line (or read it from
the guest), and only then re-run the scan. Until that is settled, do not treat
"3584 blocks were never written" as a write-path finding — it is exactly the
shape of the traps this file already records, where a confident number was taken
from the wrong device.
