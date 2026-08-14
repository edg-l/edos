# Where EDOS could be novel, and where it should not bother

Written because the libc work is the first thing that actively pulls this
kernel's ABI toward Linux's, and "are we just building a worse Linux" is a fair
question to ask before stage 2 grows the first irreversible struct.

The short answer is at the end of section 4. Sections 1–3 are the evidence.

The framing that matters, stated up front: **adopting Linux-shaped details is
not what makes a system a clone. Having no idea of its own is.** The strongest
counter-example is the closest peer this project has, and it is section 2.1.

## 1. What EDOS already is

Measured, not remembered. This is the part a literature survey would skip and
it is the part that decides the answer.

### 1.1 It is already spawn-first, and `fork` is nearly vestigial

Across `programs/`: **34 `spawn` call sites against 16 `fork` sites, and 13 of
the 16 are in tests and benchmarks** (`forktest`, `mmaptest`, `sigtest`,
`switchbench`, `inflighttest`). The real users are two:

- `edos-sh` forks twice — once for a background job so it can `setpgid` into its
  own group, once for a subshell so `exit` ends the subshell.
- `strace` forks once.

`edos-init`, the only process the kernel starts, uses `spawn` exclusively for
all four services it supervises. `SYS_SPAWN2` takes a struct carrying path,
argv, envp and the three descriptors — which is `posix_spawn`'s shape, not
`fork`'s.

So EDOS did not inherit the Unix process model; it grew a `posix_spawn`-shaped
one and bolted `fork` on beside it. That is a real design position that nobody
wrote down.

### 1.2 The kernel holds mechanism and userspace holds policy, unevenly

The window system is the clearest case: the kernel owns a window registry and
routes input, but **it does not know what a title bar is** — that moved to
userspace during the shell rebuild, along with per-window frames. Meanwhile the
kernel still owns the page cache, the journal, five filesystems, three network
transports, xHCI, AHCI and HDA. It is a monolithic kernel with one unusually
clean seam in it.

### 1.3 It is a monolith with a large unsafe surface

**824 `unsafe` occurrences across 50,016 lines** of kernel Rust. Rust is used as
a better C here, not as an isolation mechanism: there is no intra-kernel
privilege boundary, and a bug in the HDA driver can corrupt the journal.

### 1.4 The ABI is drifting Linux-ward, deliberately and recently

`SYSCALL`/`SYSRET` with Linux's register convention, a SysV initial stack with a
real auxv, negative-errno returns in POSIX numbering, `mprotect`, `O_*` flags at
Linux's values, `AT_FDCWD`. All of that landed in the last day. None of it was
forced; each was chosen because a libc needed it.

## 2. The field, grouped by the question it answers

### 2.1 "Can you be Linux-compatible and still be novel?" — Asterinas

The most important entry, because it is the closest peer by every measure:
Rust, x86-64, **over 100K lines**, roughly EDOS's scale and vintage.

Asterinas is **fully Linux ABI-compatible** — over 210 syscalls, ext2, exFAT,
overlayfs, TCP/UDP/Unix sockets — and it is nobody's idea of a clone, because
its novelty is structural rather than interfacial. Its **framekernel** puts an
intra-kernel privilege boundary inside a single address space: a small
privileged framework (OSTD) may use `unsafe`, and every de-privileged OS service
must be written in safe Rust. The result is a memory-safety TCB of **14.0% of
the codebase** at performance on par with Linux.

That is the direct answer to the worry. Asterinas gave away the entire ABI
question and kept all of its identity, because it had one.

Against EDOS's 824 `unsafe` sites in 50k lines, this is also the sharpest
available critique of the current structure.

### 2.2 "What if the language is the isolation mechanism?" — Theseus, RedLeaf, Singularity

**Theseus** (OSDI '20) is the most philosophically ambitious: many tiny
components with runtime-persistent bounds that **hold no state for each other**,
and an *intralingual* design that hands OS invariants to the compiler to
enforce. Its target is **state spill** — one component's state changing
lastingly because another interacted with it — and eliminating it is what buys
live evolution and fault recovery of core components, reportedly without a
glaring performance penalty.

**RedLeaf** (OSDI '20) is the pragmatic sibling: lightweight language-based
*domains* with **no hardware address spaces at all**, dynamically loadable and
cleanly terminable, communicating through `RRef<T>` — a value on a shared heap
under an ownership discipline enforced across domain calls, which is what lets a
crashing domain's objects be reclaimed safely. It demonstrates end-to-end
zero-copy with fault isolation and transparent driver recovery.

**Singularity** (MSR) is the ancestor of both: software-isolated processes and
contract-based channels.

The common claim: if the compiler guarantees safety, hardware isolation is a
tax, and dropping it makes isolation cheap enough to use at a much finer grain.

### 2.3 "What if the process model is wrong?" — the fork critique

**"A fork() in the road"** (HotOS '19, Baumann, Appavoo, Krieger, Roscoe) argues
fork was a clever hack for 1970s machines that is now a liability: it stopped
being orthogonal and now infects every other abstraction; its inherit-by-default
behaviour **violates least privilege**; and it is slow in practice — Chrome sees
up to 100 ms in `fork`, Node.js blocks for seconds forking before exec. The
recommendation is to deprecate it and teach it as a historical artifact.

Read section 1.1 again against that. EDOS is already most of the way there by
accident.

### 2.4 "What if the kernel should multiplex hardware and nothing else?" — exokernel, unikernels

The **exokernel** line and its descendants (MirageOS, Drawbridge) push policy
into a library OS linked into the application. It is the strongest available
argument against EDOS's current shape, and also the least compatible with
wanting to run other people's software.

### 2.5 "What if handles replace the global namespace?" — seL4, Zircon

**seL4**'s capabilities plus formal verification, and **Zircon**'s
object-capability handles instead of integer descriptors in a global namespace.
The relevant EDOS question is whether the fd table is the right abstraction.

### 2.6 "Is scheduling policy even the kernel's business?" — sched_ext, ghOSt, and the microsecond line

The most active of these lines, and the one that fits EDOS best.

**sched_ext** is in mainline Linux: a BPF scheduler implementing
`struct sched_ext_ops` can be loaded, switched and unloaded at runtime, with the
kernel restoring default behaviour the moment a task stalls or an error is
detected. **ghOSt** goes further and delegates policy to a *userspace* agent
over shared memory, with BPF carrying the events. Both start from the same
premise: the scheduler is policy, policy changes per workload, and compiling it
into the kernel was never necessary.

The microsecond line attacks a different axis. **Shinjuku** (NSDI '19) makes
preemption practical **every 5 µs** using hardware virtualization support,
reporting up to 6.6× throughput and 88% lower tail latency against IX and ZygOS.
**Shenango** and **Caladan** (OSDI '20) reallocate cores between applications on
microsecond timescales based on queueing delay, Caladan through a centralised
scheduler plus a kernel module that bypasses Linux's.

One number in `doc/SCHED-ROADMAP.md` sits directly against this:
`MIN_TIMER_INTERVAL` is an admittedly picked 10 µs. The flat 5 ms timeslice that
used to sit beside it is gone — the slice is a per-thread request now, defaulting
to 1 ms — but Shinjuku's region is still three orders of magnitude below that,
and the reason is measurable here rather than a matter of ambition: arming the
APIC one-shot costs ~1 µs because a hypervisor traps it, so a 5 µs quantum would
spend a fifth of the machine on its own timer.

### 2.7 What the current literature is actually about

Worth saying plainly, because it changes what is worth reading. OSDI '25 and
SOSP '25 are dominated by datacenter and LLM-serving concerns — GPU sharing,
inference scheduling, memory offloading, SmartNIC offload. That is where the
funding is, and almost none of it transfers to a workstation-shaped hobby OS.

The consequence: for EDOS the *structural* work of 2019–2021 is still the live
reading, and the newest directly relevant hardware lever is **user-level
interrupts** (Intel UINTR), which delivers an interrupt to userspace without a
kernel transition. That is a genuine opportunity and is gated on hardware EDOS
cannot currently test against, so it is a note rather than a plan.

## 3. The five questions, answered

### 3.1 Does EDOS want errno at all?

**Yes, keep it.** This one is already settled by evidence rather than taste: the
kernel moved to negated-errno returns in POSIX numbering this week, and the
newlib port immediately proved the limit of that choice — newlib has its own
numbering, so 18 of 54 codes still need translating
(`libs/libgloss-edos/README.md`).

A richer typed-error channel is a real design space, but every libc and every
piece of third-party software wants an integer, so a novel scheme buys a
translation layer at every boundary and nothing else. Spend the novelty
elsewhere.

### 3.2 Does EDOS want `fork`?

**No, and it nearly does not have it.** This is the strongest novelty
opportunity in the list because the work is small and the position is already
half-taken.

What stands in the way is two `edos-sh` features: background jobs and subshells.
Both are solvable without `fork` — a background job needs `spawn` plus a
`setpgid` at spawn time (which `SpawnArgs` could carry as a field), and a
subshell needs the shell to re-exec itself with a marker rather than clone its
address space.

Removing `fork` would delete the COW machinery, `clone_user_page_tables_cow`,
the `COW_BIT` fault path — the source of
`doc/bugs/2026-08-15-cow-granted-write-a-vma-refused.md` — and the pgid
inheritance bug that is still open. That is a large, load-bearing subsystem
removed in exchange for two shell features, and it makes a defensible claim:
*EDOS is a Unix-shaped system with no `fork`.*

The honest cost: `fork` is what a lot of ported C software expects, and stage 2
of the libc work is about running ported C software. Those pull in opposite
directions and the decision cannot be deferred past stage 2.

### 3.3 Is the fd table the right abstraction, or handles/capabilities?

**Keep the fd table.** Capabilities are the right answer for a system whose
selling point is security, and EDOS's is not. The migration would touch every
syscall and every program, and the benefit — least privilege by construction —
is not something anything in the tree currently wants. Revisit only if EDOS ever
wants to run untrusted code.

Worth taking the cheap half: `fork`'s inherit-by-default is the concrete
least-privilege violation here, and 3.2 removes it without a capability system.

### 3.4 Where could the type system replace a hardware mechanism?

**This is the one worth pursuing, and Asterinas shows the affordable version.**

Theseus and RedLeaf are research systems that abandoned hardware isolation
entirely; that is not a change a working system with 118 programs can make. The
framekernel is incremental: draw a line inside the kernel, put the code that may
write `unsafe` on one side, and require safe Rust on the other. It does not
change the ABI, does not change userspace, and can be done subsystem by
subsystem.

EDOS's 824 `unsafe` sites are the argument for it. A first cut costs nothing but
discipline: identify which of them are genuinely privileged (page tables, port
I/O, MMIO, the frame allocator, context switch) and which are incidental, then
put the incidental ones behind safe wrappers the rest of the kernel uses.
`doc/invariants/lock-order.md` shows this project can hold a global invariant
across the tree, so the discipline is plausible here.

### 3.5 Should the scheduler be the place EDOS is original?

**Yes, and it is the best target in this document.** The argument is one
sentence: *everything this doc recommends conceding is ABI, and the scheduler is
the largest thing in the kernel that is not.*

Errno numbers, struct layouts and flag values are compatibility surface — a
program can tell the difference, so being different costs software. Scheduling
policy is invisible across the syscall boundary. EDOS can do something nobody
else does there and lose nothing, which is true of no other subsystem.

Three things make it concrete rather than aspirational:

- **There is real, measured work waiting.** `doc/SCHED-ROADMAP.md` and
  `doc/AUDIT.md` §4 named four defects, and three are now closed: load was
  measured as `thread_count`, so a sleeping thread weighed the same as a
  CPU-bound one; the timeslice was a flat 5 ms regardless of priority, on top of
  priority buckets that could starve their bottom level outright; and the idle
  loop polled for steals on a backoff instead of being told by an IPI. What is
  left is priority inheritance, so a high-priority waiter still queues behind a
  low-priority lock holder. Parked threads never migrate, either.
- **The instruments exist.** `programs/switchbench` and `/proc/sched_prof`,
  with the single-CPU-boot discipline the roadmap already documents.
- **EDOS is less constrained than Linux is.** sched_ext has to coexist with
  EEVDF and live inside the BPF verifier. EDOS has neither obligation and could
  make the scheduler a replaceable component *by construction* rather than as a
  bolted-on second class.

This is also the natural extension of the principle EDOS already follows in the
window system and has never written down: the kernel holds mechanism, userspace
holds policy. A scheduler is the purest case of policy in the entire kernel, and
it is currently the least policy-separated thing in it.

## 4. Recommendation

**EDOS's identity should be structural, not interfacial.** Copy Linux's ABI
without embarrassment — Asterinas does, at 100K lines and full compatibility,
and is a serious research system — and spend the originality budget on things a
user can feel and a reader can point at.

In order:

1. **Make the scheduler a replaceable component, and then do something with
   it.** The only subsystem where being different costs no compatibility, with
   four measured defects already written down and the instruments to judge a fix
   by. Start by fixing what the roadmap names — load as a real quantity rather
   than `thread_count` is the one that changes behaviour most — and take the
   structural step at the same time: a scheduler behind an interface, chosen at
   boot or at runtime, so a policy is a thing you write rather than a patch.
2. **Delete `fork`.** Highest ratio of claim to work in this document, and the
   only item where EDOS is already most of the way to a position the literature
   argues for. Removes COW, a fixed security bug's whole class, and an open pgid
   bug. Decide before stage 2 of the libc work, because ported C software is the
   counter-pressure.
3. **Draw a framekernel line.** Not a rewrite: an audit of the 824 `unsafe`
   sites into privileged and incidental, and a rule that new code outside the
   privileged set is safe Rust. Measure the ratio and publish it; Asterinas's
   14.0% is the number to be compared against.
4. **Name the mechanism/policy seam.** The window system already follows it and
   nobody has written it down as a principle. Item 1 is the biggest instance;
   writeback thresholds and readahead are the next candidates.

### What to say no to

- **A capability system.** Right for a security-focused OS. EDOS is not one, and
  the cost is every syscall and every program.
- **Abandoning hardware isolation** (RedLeaf's position). Correct research,
  wrong for a system that already runs 118 programs and wants to run others'.
- **Exokernel/library-OS restructuring.** Directly opposed to the goal of
  running third-party software.
- **A novel error-reporting scheme.** Section 3.1.
- **Formal verification.** seL4-scale effort for a kernel that is still growing
  subsystems weekly.

### The thing to stop worrying about

Errno numbers, `O_*` flag values, `struct stat` layout, the SysV stack, auxv.
These are lookup tables. None of them is where an operating system's character
lives, and matching them is what lets other people's software run. The
irreversible ones are worth care in the order stage 2 introduces them — but care
means *choosing deliberately*, not refusing.

## Sources

- [Asterinas: A Linux ABI-Compatible, Rust-Based Framekernel OS with a Small and Sound TCB](https://arxiv.org/abs/2506.03876) (USENIX ATC '25), and [the project](https://github.com/asterinas/asterinas)
- [Theseus: an Experiment in Operating System Structure and State Management](https://www.usenix.org/system/files/osdi20-boos.pdf) (OSDI '20)
- [RedLeaf: Isolation and Communication in a Safe Operating System](https://mars-research.github.io/doc/2020-osdi-redleaf.pdf) (OSDI '20)
- [A fork() in the road](https://www.microsoft.com/en-us/research/uploads/prod/2019/04/fork-hotos19.pdf) (HotOS '19)
- [ghOSt: Fast & Flexible User-Space Delegation of Linux Scheduling](https://cs.stanford.edu/~jhumphri/documents/ghost.pdf) (SOSP '21), and Linux's [Extensible Scheduler Class (sched_ext)](https://docs.kernel.org/scheduler/sched-ext.html)
- [Shinjuku: Preemptive Scheduling for µsecond-scale Tail Latency](https://www.usenix.org/system/files/nsdi19-kaffes.pdf) (NSDI '19)
- [Caladan: Mitigating Interference at Microsecond Timescales](https://www.usenix.org/conference/osdi20/presentation/fried) (OSDI '20)
