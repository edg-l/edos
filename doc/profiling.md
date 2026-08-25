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
    758   66.0%  [idle] <edos_kernel::thread::scheduler::Scheduler>::pick_and_run
    267   23.3%  <sha256sum::Sha256>::compress
     40    3.5%  edos_kernel::util::uaccess::do_user_copy
     17    1.5%  <edos_kernel::drivers::nvme::queue::NvmeQueue>::write_sqe_and_ring
     14    1.2%  memcpy
```

That is a single-threaded `sha256sum` on a four-CPU guest, and every line of it
is worth reading as an example of how to read one. The workload's own hot loop
is a quarter of the samples; the idle loop is two thirds, because three CPUs had
nothing to do; and the remaining few percent are the read path feeding the hash,
which is the part nobody would have thought to instrument.

The `self` column is the leaf of each stack — where the CPU actually was. Use
`--folded` when the question is which *path* led there.

**`[idle]` is not the scheduler being slow.** A halted CPU is halted *inside*
`run_idle`, which `pick_and_run` calls, so an idle machine reports almost all of
its time under that symbol — 99.9% of samples on an idle desktop, in one stack.
The tag is what separates it from the scheduler doing real work, and a row
without the tag is the only one worth reading as scheduler cost.

## Reading one honestly

**A profile is on-CPU time, not wall-clock time.** A program blocked on I/O
contributes nothing and does not appear. A program that takes ten seconds and
spends nine of them waiting shows up as one second of work. If the question is
"why is this slow" and the answer is not in the profile, that is itself the
answer: it was not running. On-CPU is not the same as executing, though — see
the MMIO paragraph below.

**Idle is data.** A four-CPU guest running one thread is 75% idle by
construction, and `pick_and_run` dominating the profile means exactly that. It
is not overhead; comparing it across runs is how you see whether a workload
actually parallelised.

**A single sample means nothing.** The instrument is the distribution of
thousands. Anything under a few hundred samples is noise with symbol names
attached.

**Under a hypervisor, an MMIO write is charged to the guest as if it ran.** The
guest's clock keeps time across a VM exit, so a tick expiring while QEMU
emulates a device register fires the moment the guest resumes and lands on the
instruction that trapped. The cost of the emulation is then attributed to the
guest function holding it. This is not small and it is not evenly spread: on
`fsbench raw` the profile above puts 9% of non-idle time in
`<NvmeQueue>::write_sqe_and_ring`, and `scripts/perf-kvm` — which counts only
instructions the guest actually executed — puts **0.04%** there, with the
missing time visible in the host half as `kvm_io_bus_*`, `kvm_fast_pio` and
`x86_emulate_instruction`. The 1.5% row in the example above is the same
artifact.

So a driver's doorbell, register poll or descriptor-ring write reads as compute
here and would not on hardware. Before optimising anything that touches MMIO on
the strength of a guest profile, check the same workload under `scripts/perf-kvm`
and see whether the guest is executing there at all.

## Two things it cannot see

Both are properties of the mechanism rather than bugs, and both are worth
knowing before trusting a profile that looks wrong.

**Code running with interrupts disabled is invisible.** The sample is taken *by*
an interrupt, so an instruction executed with them off can never be the
interrupted one. Every `without_interrupts` body and every `IrqSpinlock` holder
is therefore under-reported, and its time is charged to whatever ran next. This
matters here more than it would in most kernels: the frame allocator's global
lock, the per-CPU heap cache and the page-table edits all live under exactly
that protection. Seeing into it needs a sampler an interrupt flag cannot hold
off, and one already exists outside the guest: `scripts/perf-kvm` below.

**A thread inside a syscall gives its kernel stack and no user frames.** A
sample lands in ring 3 or in ring 0 and reports the stack it interrupted, with
no attempt to stitch the two halves together. So a user-mode profile shows where
a program computes, not which of its call sites entered the kernel. The kernel
half is still attributed to the right thread, which is usually the question.

## Sampling from the host, where an NMI can reach

`scripts/perf-kvm` profiles a running guest from outside it, and it exists for
exactly one reason: it sees the interrupts-off code the guest's own sampler
structurally cannot. A cycles overflow on the host PMU arrives as an NMI, an
NMI leaves the guest whatever `RFLAGS.IF` says, and KVM records the interrupted
guest RIP. The guest's CPU model is irrelevant — nothing here needs a
performance counter *inside* the guest.

```bash
scripts/edos-vm start
scripts/perf-kvm -d 10          # while a workload runs in the guest
```

Symbols come from `nm` over the kernel ELF in this tree, written out in
/proc/kallsyms format. **The synthesized file must carry a `_text` symbol**:
`perf record` places the guest kernel map from a reference relocation symbol it
looks up by that name, and without it the recording contains no guest map at
all — every guest sample stays a bare address, and `report` cannot repair it
afterwards however good the kallsyms file is.

### What it found

`fsbench raw /dev/nvme0n1` under both instruments at once, three runs:

| symbol | in-guest `profile` | `perf-kvm` |
|---|---|---|
| `<buddy_system_allocator::Heap<32>>::dealloc` | 0 samples | 3.9 / 5.7 / 7.0% |
| `Allocator::try_percpu_dealloc::{closure#0}` | 0 samples | 0.5 – 0.6% |
| `Allocator::try_percpu_alloc::{closure#0}` | 0 samples | 0.5 – 0.7% |
| `BitmapFrameAllocator` (three symbols) | 0 samples | 0.5 – 0.6% |
| `Allocator as GlobalAlloc::{alloc,dealloc}` | 1.8% | 1.6 – 1.7% |

The split is not approximate, and it is not noise. The kernel heap's `Heap<32>`
lives inside an `IrqSpinlock` and `try_percpu_alloc`/`dealloc` run inside
`without_interrupts` (`kernel/src/allocator.rs`), so the rows that appear only
under `perf-kvm` are precisely the code the compiler placed in the shadow. The
outer `GlobalAlloc` wrappers, which run with interrupts on, are the control:
they agree between the two to within a fraction of a percent. The allocator is
6–8% of guest cycles on that workload and the guest's own profiler reports none
of it.

### What it cannot do

**No stacks.** Every guest sample is one frame deep. The host kernel does not
walk a guest stack, and `--call-graph fp` changes nothing about that;
`--guest-code` (which asks perf to find guest code in the hypervisor process)
does not help either and loses symbol resolution outright. Use the in-guest
profiler when the question is which path led somewhere.

**No thread identity.** perf sees vCPU threads, not guest threads, so nothing
distinguishes two guest programs running on one CPU. Boot `--smp 1` and run one
workload at a time when the attribution has to be trusted.

**Guest userspace is sampled but not resolved.** The user RIPs are there and
they are real — `--user` lists them — but with no thread identity nothing can
say which program an address belongs to. Given the program, the same
`addr2line` the guest profiler uses will name it, at the PIE load base:
`addr2line -e programs/target/x86_64-unknown-edos/debug/<prog> -f -C -i
$((addr - 0x400000))`.

### The two denominators are different, so do not compare shares across them

The in-guest profiler counts guest *wall-clock* time per CPU, halted CPUs
included — which is why an idle machine reports 99.9% in `pick_and_run`.
`perf-kvm` counts cycles the guest actually executed, so a halted CPU
contributes nothing at all and idle never appears.

Time spent in VM exits splits the same way and in the opposite direction. A
port or MMIO access the guest thinks is an instruction is *host* time to perf,
so the guest's own numbers carry an emulation bill that never appears in the
guest half of a `perf-kvm` capture. It is in the host half, which the recording
keeps and the script's report filters out; save it with `-o` and read it with

```bash
perf report -i out.data --stdio -s sym --comms qemu-system-x86
```

On the `fsbench raw` capture that is `svm_vcpu_run` at 3.1% of all machine
cycles plus `kvm_io_bus_*` and `emulator_pio_in_out` under it, and on an idle
guest it is the one-shot timer being re-armed, `kvm_lapic_reg_write` into
`set_target_expiration`. Neither is guest overhead and neither is the guest's
to fix; both are what the guest costs to run here.

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
