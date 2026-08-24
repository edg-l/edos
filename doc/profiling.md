# Profiling

`profile` in the guest samples every CPU on a timer and writes down where it
was. `scripts/profile-resolve` on the build host turns those addresses into
symbols. Together they answer the question no other instrument in this tree
answers: **where does the time go**, without having to guess first.

Everything else here measures something already suspected. `sched_prof` times
nine named stages of the switch path. `switchbench`, `fsbench`, `latbench`,
`balancebench` and `allocbench` time whole operations. `/proc/*_stats` count
events. Each is a probe placed by somebody who already had a hypothesis; the
profiler is the one that produces the hypothesis.

## Using it

```
profile [-f HZ] [-d SEC] [-o FILE] [PROGRAM [ARGS...]]
```

With a `PROGRAM`, sampling runs for exactly that program's life. Without one,
the whole machine is sampled for `-d` seconds (default 5). Either way sampling
is **machine-wide**: the kernel samples every CPU, so the profile contains
everything running, not only the program named. That is deliberate — a program
that is slow because another one is holding a lock is a profile you cannot read
if you filtered the other one out.

Headless, the profile goes out over the serial log and comes back on the host:

```bash
scripts/edos-vm type 'profile -f 999 sha256sum /bin/edos-web > /dev/klog 2>&1' --enter
scripts/profile-resolve run_log.txt
scripts/profile-resolve run_log.txt --folded | flamegraph.pl > profile.svg
```

`make profile-check` is the gate: it runs a workload whose hot function is known
in advance and fails unless the profile names it.

## What comes out

```
1137 samples at 999 Hz (1137 taken, 0 dropped)

   self       %  symbol
    734   64.6%  <edos_kernel::thread::scheduler::Scheduler>::pick_and_run
    280   24.6%  <sha256sum::Sha256>::compress
     32    2.8%  edos_kernel::util::uaccess::do_user_copy
     26    2.3%  memcpy
     17    1.5%  <edos_kernel::drivers::nvme::queue::NvmeQueue>::write_sqe_and_ring
```

That is a single-threaded `sha256sum` on a four-CPU guest, and every line of it
is worth reading as an example of how to read one. The workload's own hot loop
is a quarter of the samples; the idle loop is two thirds, because three CPUs had
nothing to do; and the remaining few percent are the read path feeding the hash,
which is the part nobody would have thought to instrument.

The `self` column is the leaf of each stack — where the CPU actually was. Use
`--folded` when the question is which *path* led there.

## Reading one honestly

**A profile is CPU time, not wall-clock time.** A program blocked on I/O
contributes nothing and does not appear. A program that takes ten seconds and
spends nine of them waiting shows up as one second of work. If the question is
"why is this slow" and the answer is not in the profile, that is itself the
answer: it was not running.

**Idle is data.** A four-CPU guest running one thread is 75% idle by
construction, and `pick_and_run` dominating the profile means exactly that. It
is not overhead; comparing it across runs is how you see whether a workload
actually parallelised.

**A single sample means nothing.** The instrument is the distribution of
thousands. Anything under a few hundred samples is noise with symbol names
attached.

## Two things it cannot see

Both are properties of the mechanism rather than bugs, and both are worth
knowing before trusting a profile that looks wrong.

**Code running with interrupts disabled is invisible.** The sample is taken *by*
an interrupt, so an instruction executed with them off can never be the
interrupted one. Every `without_interrupts` body and every `IrqSpinlock` holder
is therefore under-reported, and its time is charged to whatever ran next. This
matters here more than it would in most kernels: the frame allocator's global
lock, the per-CPU heap cache and the page-table edits all live under exactly
that protection. The fix is an NMI-based sampler, which needs a performance
counter; the `qemu64` model this tree boots (`GNUmakefile`) exposes none, so it
would mean changing the CPU model to `host` and only working under KVM.

**A thread inside a syscall gives its kernel stack and no user frames.** A
sample lands in ring 3 or in ring 0 and reports the stack it interrupted, with
no attempt to stitch the two halves together. So a user-mode profile shows where
a program computes, not which of its call sites entered the kernel. The kernel
half is still attributed to the right thread, which is usually the question.

## How it works

**Timing.** There is no second clock. The LAPIC one-shot is already armed to
whichever comes first of the running thread's slice end and the next sleeper's
expiry (`Scheduler::arm_timer_until`), and a sample deadline is simply one more
thing it must not fire after. `clamp_to_sample` brings the requested deadline
forward, and the existing rule — never skip a write that would let the timer
fire late — does the rest unchanged. This is why an idle, halted CPU is sampled
at the full rate: its idle loop re-arms through the same function.

The one consequence worth knowing: a CPU halted at the moment a session starts
keeps whatever deadline it already had, up to 100 ms out, so the first fraction
of a second under-counts *idle* on such CPUs. Undercounting idle is the harmless
direction, and nothing re-arms a halted CPU without an IPI it does not otherwise
need.

**Walking.** The kernel is built with `-C force-frame-pointers=yes`
(`kernel/GNUmakefile`), and since this work so is the userspace workspace
(`programs/.cargo/config.toml`). So `rbp` really is the head of a linked list
and the walk is exact rather than heuristic. Without the flag LLVM omits frame
pointers in optimized code, only about half of a Rust binary's functions leave
one behind, and a walk reads whatever happened to be in the register and invents
callers.

A kernel walk is bounded to the kernel stack the interrupted code was standing
on, so every read is inside a range that is mapped by construction and no read
can fault. A user walk goes through `read_u64_nofault`, which holds a
`NoFaultGuard` — this tree's `pagefault_disable()`: while it is held, a ring-0
fault on a user address takes the fixup path at once instead of being demand
paged, because filling a page blocks and this runs inside the tick.

Reaching the outermost frame is how a walk *normally* ends, and is not flagged.
Only a walk that found no caller at all is, which means the code was interrupted
between a function's `push rbp` and the `mov rbp, rsp` after it.

**Transport.** A per-session ring of 4096 samples, allocated on claim and freed
on release, behind an `IrqSpinlock` that is deliberately unranked: the rank stack
is per thread and an interrupt handler would push onto the stack of whatever
thread it interrupted, so ranking it would report an inversion against every lock
that thread legitimately holds. It is a true leaf — the sample path takes it,
copies one struct, and releases it without calling anything. A full ring refuses
the new sample and counts it, so a profile always says how much of itself is
missing.

`SYS_PROFILE_CTL` (318) and `SYS_PROFILE_READ` (319) are the session's start,
stop and drain, shaped after the tracer's pair in `syscalls/trace.rs`. The record
layout lives in `libs/edos-profile-abi`, which both sides link, so it cannot
drift.

**Resolving.** The guest resolves nothing, on purpose: the kernel and every
program are ordinary ELF files in this tree with their DWARF intact, and
`addr2line` already reads them on the host. Carrying a symbol table into a
`no_std` kernel would be work spent to answer a question the build machine
answers for free. The loader places images at a fixed base — 0 for `ET_EXEC`,
`0x400000` for a static PIE (`kernel/src/loader/mod.rs`) — so there is nothing to
relocate against and a sample needs to carry only a thread id. The profiler reads
that thread's command line out of procfs to name the binary.

## When this is the wrong instrument

- **Below a microsecond.** Sampling at 999 Hz cannot see a 200 ns function.
  `switchbench` and `/proc/sched_prof` are for the switch path; `hyperfine` and
  the bench programs are for anything with a repeatable inner loop.
- **When the question is a count, not a share.** `/proc/block_cache`,
  `/proc/nvme_stats` and their neighbours answer "how many" exactly, where a
  profiler only estimates.
- **When the program misbehaves rather than runs slowly.** `strace` first; see
  `doc/strace.md`.
