# The hostile NVMe boot is two bugs, and neither one is the log (2026-08-26)

## Status

**Not fixed.** This writeup corrects the diagnosis and leaves two separate
defects open, each now with a signature that tells it apart from the other in
one command. The instruments are `scripts/wedge-probe`, `scripts/wedge-resolve`
and `edos-vm --on-reset pause --qemu-log`.

## Symptom

`make nvme-check`'s watchdog case boots `edos-nvme-hostile.iso`, whose cmdline
carries `nvme_timeout_ms=0`. Intermittently the boot fails its assertions: the
serial log simply stops, somewhere between 0.19 s and 15 s of guest time, and
nothing more is ever written. The gate waits 300 s and reports that `init: pid`
never appeared, or that no controller reset completed.

Measured 2026-08-26 with a fresh `nvme-disk.img` per run: **3 failures in 9
runs**. The 2026-08-25 measurement over 30 runs put it at 7 in 30.

## What the record said, and why it was wrong

The standing entry described a shape where "the kernel's log path died while the
guest lived", resting on two observations: the log stops mid-word, and
`init: pid` is present, so "userspace kept writing to serial afterwards".

Both readings are wrong, and the second is the one that matters.

**`init: pid` is not after the truncation.** In every kept log it appears
*before* the last kernel line. Its presence proves only that init started, not
that anything survived. Nothing at all follows the truncation point; the file
simply ends.

**Stopping mid-word carries no information.** A *passing* run ends mid-line too.
`log!` builds the whole line into a `String` before it is queued, so the string
the klogger hands to the UART is always complete; a partial line just means the
guest was stopped while the klogger was inside `write_str`. Under this workload
the klogger is inside `write_str` most of the time.

## What the evidence says

The question the log cannot answer is whether the machine is still there. QMP
can, and it needs no kernel change. `scripts/wedge-probe` watches the serial log
for silence and then asks QEMU. The answers split cleanly into two shapes.

### Shape B: the guest resets, and it does not panic

`query-status` answers `Connection refused`: **QEMU is gone.** `edos-vm` passes
`-no-reboot`, so a guest that resets takes QEMU with it.

It was not a panic. `rust_panic`'s first statement, before it touches a lock, a
scheduler or the heap, is

```rust
crate::serial::emergency_write(b"\n!!! KERNEL PANIC !!!\n");
```

which spins straight on the UART's THRE bit and cannot be blocked by anything
the rest of the kernel holds. It then ends in `loop { hlt() }`, which halts and
leaves QEMU running. **No failing log contains that marker**, and QEMU exited.
So the guest reset without any handler reaching serial: a triple fault, or a
fault taken where the double-fault handler could not run.

Both shape-B captures land in the same three log lines of AHCI controller
discovery, at 0.185 s to 0.19 s, with zero watchdog firings; a healthy boot's
control log shows `Device ID`, `BAR5` and `IRQ Line` at that exact point. Two
independent failures landing inside three lines is not a uniform wedge point.

### Shape A: the guest is alive, and nothing is runnable

`query-status` answers `running`, and `info registers -a` puts **all four
vCPUs at the same RIP**:

```
RIP=ffffffff80049b02  Scheduler::take_idle  (scheduler.rs:1185)
                      <- Scheduler::run_idle (scheduler.rs:735)
```

The machine is fine. It has nothing to run. Every thread, the klogger and the
NVMe watchdog included, is parked, which is why the log is silent: nothing is
running to produce a line.

This is **not** the 2026-08-19 "idle CPU halted with a runnable thread" bug, and
the difference is the discriminator. There, a runnable thread existed and the
100 ms fallback timer in `run_idle` eventually found it, which bounds that
symptom at ~100 ms. Here the silence runs for the full 300 s, so the fallback
fires and finds nothing: there is genuinely no runnable thread. Something is
waiting on a completion that a controller reset abandoned and never re-signalled.

The first place to look is the open remainder of the block-layer retry work:
`block_page_cache.rs`'s writeback batch and `page_fill.rs`'s prefetch runs are
the two paths still waiting on bare handles rather than through the retrying
`block_io::{read,write,flush}_blocking`.

## If this reappears

Do not read anything into where the log stops. Ask QEMU instead:

```bash
scripts/edos-vm qmp query-status
```

- `Connection refused` -> shape B. The guest reset. Confirm no `KERNEL PANIC`
  marker in the log, then boot with `--on-reset pause` so the VM freezes at the
  fault instead of taking QEMU with it, and resolve the RIPs.
- `"status": "running"` -> shape A. Dump the vCPUs and resolve them:

```bash
scripts/edos-vm qmp human-monitor-command '{"command-line":"info registers -a"}'
scripts/wedge-resolve <dump>
```

  All CPUs in `run_idle`/`take_idle` means nothing is runnable, and the question
  is which wait never returned, not why the log stopped.

Run the whole thing with `scripts/wedge-probe N`, which does the above per run
and keeps every artifact. **Give every run a fresh `nvme-disk.img`**: the guest
writes to it and the case's duration is set by how much journal there is to
replay, so consecutive runs against one image measure an increasingly churned
filesystem rather than a kernel.

## The real lead: the kernel heap free list is corrupt

A 20-boot run caught a third outcome the earlier captures had not: a guest that
**panicked**, so the handler's own output is in the log.

```
GPF Error code: 0x0
Selector index: 0
Table: GDT
External: false
Stack frame: InterruptStackFrame {
    instruction_pointer: VirtAddr(0xffffffff800c99a7),
    code_segment: SegmentSelector { index: 1, rpl: Ring0 },
```

```
$ addr2line -e kernel/target/x86_64-unknown-none/debug/edos-kernel -f -C 0xffffffff800c99a7
<buddy_system_allocator::linked_list::LinkedList>::pop
.../buddy_system_allocator-0.13.0/src/linked_list.rs:53
```

A #GP in ring 0 with **error code 0** is not a selector fault; at a memory
access it is a non-canonical address. `LinkedList::pop` walking a garbage
pointer means **the buddy allocator's free list is corrupt**, which is a
use-after-free or double-free of kernel heap memory, not anything about NVMe
queues.

That reframes both shapes as symptoms of one defect. Corruption that lands on a
free-list pointer gives this #GP; land it somewhere the fault handler cannot
survive and you get shape B's triple fault; corrupt something a wake path needs
and you get shape A's parked machine. It also explains why shape B always dies
in the same three lines: that is simply where the boot is when the allocator is
hottest.

The hot suspects under `nvme_timeout_ms=0` are the op payload's DMA buffers.
The reset path's own comment says `DmaBuffer` **has no `Drop`** and that
clearing slots any other way "would strand each op's bounce buffer and PRP list
page", so the freeing of those pages is manual and by hand on every retire, on
a path the watchdog and the completion dispatcher both reach.

**Next step**, and it is cheap: this is what a heap canary or a quarantine
allocator is for. Add a debug-only guard page or a poison-on-free to the kernel
allocator and re-run `scripts/wedge-probe`; a poisoned pointer faults at the
free that is wrong rather than at the next unrelated `pop`.

## Three refuted hypotheses, and the instrument each needed

All three looked strong. Each was killed by a number rather than by an
argument, and the numbers are what to trust when a fourth candidate appears.

| hypothesis | instrument | result |
|---|---|---|
| a command installed across a reset is never failed | post-reset pass that logs each one | fired **0** times in 20 boots; shape A still occurred |
| the heap is corrupted by a double free | `heap-poison`: magic in each freed block's tail, panic on a second free | fired **0** times; race intact at 3/20 |
| the device DMAs into a page the watchdog already freed | quarantine: withhold those pages from the allocator | **5**/20, no better than baseline |

The middle row is the one worth reading twice, because the first attempt at it
lied. Poisoning the whole block on free took the failure from roughly one boot
in five to **none in twenty**, which reads exactly like a fix. It was not: a
memset per free is enough extra work to move the race. Dropping the memset and
keeping only the eight-byte tail stamp restored the failure rate to 3/20 and
proved the detector silent. **A clean run is only evidence if the failure still
happens when the instrument is present.**

The third is the one that felt most certain. The watchdog fails commands, and
so frees their bounce buffers and PRP pages, while the controller is still
running: `CC.EN` is not cleared until `reset_controller` several steps later.
Handing those pages back with the device possibly still writing to them is a
use-after-free whose writer is not a CPU, which would explain why no CPU-side
check sees anything. Leaking them deliberately changed nothing.

So the corruption is real -- the `LinkedList::pop` #GP is not ambiguous -- and
it is not any of: a stranded NVMe command, a double free, or device DMA into
reclaimed pages.

### What `heap-poison` is, and that it is kept

`kernel/Cargo.toml` gains a `heap-poison` feature, off by default. It stamps
`0xDEAD_F2EE_DEAD_F2EE` into the last eight bytes of every freed block and
panics on a free that finds it already there, naming the address, size and
alignment while the second caller is still on the stack. The stamp is at the
*end* because both free lists here, the buddy heap's and the per-CPU cache's,
thread their link through the *start*.

It is kept because it is now a measured instrument rather than a guess: two
accesses per alloc/free, and the failure rate under it matches stock. Reach for
it first the next time this kernel corrupts its heap; it answers "is this a
double free?" in one 20-boot run.

## Refuted: the reset window (2026-08-26)

The first hypothesis was the reset path's unguarded window against submitters,
and it is **wrong**. It is worth recording because the reasoning looked strong.

The window is real: `begin_restart` is a `compare_exchange` on one `AtomicBool`
that nothing outside the watchdog reads, so it serialises resets against resets
and not against submitters, and the reset path's own comment names the case.

It is not what happens. A 33-line change added `is_restarting()`, made
`submit_transfer` and `submit_flush` refuse to issue during a restart, and
added a post-reset pass that failed anything installed across the reset and
**logged each one**. Over 20 boots, hundreds of resets each, that line fired
**zero times**: no command is ever installed across a reset. Shape A still
occurred (run13). The change was reverted.

The lesson is the log line rather than the fix. A speculative fix that also
counts how often its window is entered answers the question even when it does
not help; without that counter, 17 passes out of 20 would have looked like a
result.

## Superseded candidate (kept for the mechanism)

Both shape-A captures end on the **same line**, `nvme: controller reset
complete`, and it is the last thing `watchdog_sweep` logs before
`end_restart()`. The reset path fails every outstanding op itself, in a loop,
and refuses to reset while any slot is still occupied, so an op that was
installed when the sweep began is handled. Its own comment names the case that
is not:

> nothing stops a submitter from installing a command between the scan and the
> reset, and that command is killed by the reset too, so it has to be failed
> here or it waits forever for a completion the controller will never post.

The loop is what answers that, but the window does not close when the loop
ends. `begin_restart` is a `compare_exchange` on one `AtomicBool`, and
**nothing outside the watchdog reads it**: `grep -rn restarting
kernel/src/drivers/nvme/` finds hits only in `admin.rs` and in
`watchdog_sweep`. It claims the right to reset against another *reset*, not
against a *submitter*. So a command installed after the final
`cmd_slots_empty()` check and before `reset_controller()` finishes is destroyed
by the reset with nothing to fail it, and whoever waits on it waits forever.

That is consistent with everything observed: one thread parks on a completion
that will never come, the mount path behind it parks, and with no runnable
thread left every CPU ends in `run_idle`. It also explains why the last line is
always the reset's, and why `nvme_timeout_ms=0` is the setting that shows it:
resets start faster than they can finish, so the window is entered constantly.

**Not yet proven.** The confirming test is a submitter blocked for the duration
of a restart (or a post-reset pass that fails whatever appeared), watched to go
red against the current code.

## The reproduction rate, and a harness trap

Interleaved A/B over 10 pairs, fresh image per run, alternating so the host
answers for both: `-no-reboot` **1 of 10**, `--on-reset pause` **1 of 10**, and
the second is a false positive. So the flag does not perturb the race, and an
earlier 12-run clean streak under `pause` was luck.

Rates measured on this bug so far: 7/30 (2026-08-25), 3/9, then 1/10. Treat it
as roughly one boot in five and re-measure rather than quoting one.

**The trap the false positive came from**: `run_log.txt` is one shared path, so
a run whose guest never started is judged against the *previous* run's log and
reported as a wedge. The only tell was two runs sharing a timestamp to the
microsecond. `wedge-probe` now truncates the log first and reports a failed
start as its own outcome.

## Saved artifacts

- `logs/2026-08-26-wedge/` -- this session. `run04` and `run09` are shape B
  (QMP refused), `run07` is shape A (QMP answering, four idle vCPUs, register
  dump in `run07-WEDGE.qmp`).
- `logs/2026-08-25-nvme-watchdog-wedge/` -- the seven logs from the 30-run
  measurement, five of them the shape-A silence and two shape B.
