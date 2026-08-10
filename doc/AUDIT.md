# Codebase audit, 2026-08-08

A read-only pass over the whole tree looking for bugs, smells, perf hot spots and
missing interfaces. ~48k lines of Rust across 205 files.

Each finding says what is wrong, where, and what the fix looks like. Severity is
my judgement of blast radius, not of effort. Where I did not confirm something
by running it, the entry says so — several plausible-looking findings were
checked and discarded (noted at the end), and the ones left standing should be
treated the same way until a test backs them.

The prioritised follow-up list, folding these in with the pre-existing work,
lives in `ideas.txt`.

---

## 1. Correctness

### 1.1 The ELF loader maps segments at an unvalidated address — FIXED (see below)

`loader/mod.rs:311`

```rust
let vaddr = base_addr + p_vaddr;   // p_vaddr comes straight from the file
```

Nothing checks that the result lands in the user half. The value flows into a
VMA insert and then into demand-fault mappings carrying `USER_ACCESSIBLE`.
`VmaSet` knows the bound — `USER_VA_END` at `memory/vma.rs:118` — but only
applies it in `first_fit` (`vma.rs:340`), the path that *chooses* an address.
Inserts at an explicit address, which is what the loader does, are unchecked.

Any user can spawn any file they can read, so the input is attacker-controlled.
Two consequences, in increasing order of how much they matter:

- A `p_vaddr` making the sum non-canonical panics in `VirtAddr::new`, so a
  crafted binary halts the kernel.
- A `p_vaddr` in the canonical higher half passes, and the loader inserts a VMA
  over kernel address space in that process, mapped user-accessible.

I did not build the crafted ELF to prove the second, so treat it as
"unvalidated input reaching a mapping decision" rather than a demonstrated
escalation. It wants fixing either way.

**Fixed.** The bound now lives in `VmaSet::insert`, which returns
`Result<(), VmaError>` and rejects a range that wraps or ends past
`USER_VA_END`; callers that hand back a range they already held (unmap
rollback, fork's deep copy, the kernel-derived TLS region) go through
`insert_validated`, which debug-asserts instead. The loader bounds
`base_addr + p_vaddr + p_memsz` with `checked_add` before it builds a single
`VirtAddr`, and rejects `p_filesz > p_memsz`, which would otherwise push the
file-backed VMA past the checked end.

The write-up above understated this. `sys_mmap` reaches the same insert with a
raw user address and never validated it either, so the halt was one ordinary
syscall away, no crafted ELF needed:

```
mmap(addr=0x0000_9000_0000_0000, len=0x1000)
  -> claim_range -> VirtAddr::new
  -> KERNEL PANIC: virtual address must be sign extended in bits 48 to 64
```

Reproduced on the pre-fix kernel from `mmaptest`, resolved to
`syscalls/memory.rs:234` via the panic backtrace. `find_free_address` was also
reachable with a length within a page of `u64::MAX`, where its align-up wrapped
to zero and returned a gap far shorter than requested; it now uses `checked_add`
and reports exhaustion as `VmaError::NoSpace` (ENOMEM) rather than panicking on
an `expect`. mmaptest test 11 covers all five cases and is the regression test.

### 1.2 CPU affinity is enforced in one place out of three — FIXED

`cpu_affinity` is a real field (`thread/thread.rs:141`) with a setter
(`:420`), and the work-stealing path honours it (`scheduler.rs:865`). The other
two paths do not:

- `Scheduler::thread_can_run_here` (`scheduler.rs:561`) is stubbed to `true`
  with the real check commented out directly beneath it.
- `Scheduler::complete_wake` enqueues on the *waker's* CPU for cache locality,
  without consulting affinity at all.

So a thread pinned away from CPU 3 still runs on CPU 3 whenever it is woken
there. Affinity currently reads as a feature but behaves as a hint that one
code path respects.

**Fixed** by enforcing it. Affinity is a *placement* property: `spawn_thread`,
`complete_wake` and work-stealing choose a CPU, and `pick_and_run` runs whatever
it pops without re-checking. All three consult `Thread::allows_cpu` now, and
`pick_sched_for` returns the least loaded CPU a thread is allowed on.

Two things this turned up that the entry above missed:

- **`spawn_thread` would have lost the thread.** Its `else` arm was a bare
  comment saying the thread "will be queued on its target cpu by that cpu's
  scheduler" — nothing did. The stub returning `true` is the only reason that
  never fired. It routes the thread now.
- **A mask naming no registered CPU** falls back to running the thread rather
  than dropping it, and `set_affinity_mask` documents that a mask set on an
  already-running thread applies at its next placement, not immediately.

`sched-test` gains `affinity-pinned` and `affinity-waker` (47 → 49 tests). The
wake rounds are what make the test discriminating: yields re-enqueue on the CPU
the thread is already on, so a yield-only test passes with affinity disabled
entirely. Reverting `allows_cpu` to `true` fails the test on the first round
with "pinned to cpu 3, ran on cpu 2 after wake 0", which was checked.

### 1.2b Load is still measured as thread count

Enforcing affinity does not change what `pick_sched_for` optimises: a sleeping
thread still weighs the same as a CPU-bound one. See §4.

### 1.3 Shared state on bare `spin::Mutex` — FIXED

`thread/preempt.rs` opens with the rule: a spin lock is only bounded while its
holder keeps running, so shared state wants `PreemptSpinlock`, and only state an
interrupt handler can reach wants `IrqSpinlock`. Several places predate it:

| Site | State |
|---|---|
| `net/stack.rs:286,304`, `syscalls/net.rs:36,37,197` | sockets and TCP connections |
| `drivers/block_io.rs:233` | `DEVICES` registry, read on every I/O |
| `graphics/mod.rs:44` | the display |
| `window/input.rs:218` | `LAST_MOUSE_BUTTONS` |

These are *not* deadlock hazards: every device IRQ handler
(`interrupts/io.rs:26-58`) only wakes a driver kthread, so none of this is
touched from interrupt context. It is a latency and starvation hazard — a
holder can be preempted and every other CPU then spins behind it, which is
exactly the failure that took a session to find in the window registry.

**Fixed.** All five converted to `PreemptSpinlock`/`PreemptRwLock`. The
deliberately unranked ones (`WaitQueue.inner`, the scheduler's `rq` and
`sleepers`, `owned_ops`) were left alone per `doc/invariants/lock-order.md`.

### 1.4 TCP measures RTT and then ignores it — FIXED, but the entry was wrong

`net/tcp.rs:588`

```rust
let rto = Duration::from_secs(1) * (1 << seg.retries.min(5));
```

The retransmit timeout is a fixed 1s base with exponential backoff, and the
connection is declared dead after 5 retries. Meanwhile `rtt_us` is measured and
stored (`net/stack.rs:364`) and used only for reporting. On a LAN with a
sub-millisecond RTT, a dropped segment costs a full second before the first
resend — roughly three orders of magnitude worse than it should be.

There is also no congestion control at all: no `cwnd`, no slow start, no SACK,
no window scaling, no delayed ACK or Nagle. Fine for a hobby stack, but it means
throughput is bounded by the retransmit policy the moment a packet is lost.

**The premise above is wrong.** The `rtt_us` at `net/stack.rs:364` belongs to
the ICMP **ping waiter**, not to TCP; `TcpConnection` had no RTT field at all.
So this was not "collected and ignored", it was never collected.

**Fixed** by adding the estimator: `srtt`/`rttvar`/`rto` per RFC 6298 section 2,
sampled when an ACK retires a segment, skipping retransmitted segments per
Karn's algorithm, `RTO = srtt + 4*rttvar` clamped to 200 ms (permitted by
section 2.4 at HPET granularity) and 60 s. Backoff now multiplies the measured
timeout rather than a fixed second.

**Unverified end to end.** No TCP connection can reach Established today — see
`doc/bugs/2026-08-08-tcp-connect-rsts-its-own-synack.md`. Congestion control
remains a separate, much larger decision.

### 1.5 A protocol mismatch in the FS request layer panics — FIXED

`fs/api.rs:31,55,63,71` — each request destructures the response it expects and
`unreachable!()`s otherwise. That makes any future mismatch a kernel panic
rather than an error return. The invariant is real today (one response type per
request), so this is about brittleness, not a live bug.

**Fixed** with `expect_ok`/`expect_partitions` in `fs/api.rs`: the pairing is
asserted in two places instead of four, and a mismatch is
`Error::ProtocolMismatch` rather than a panic. `list_partitions` returns a
`Result` accordingly.

### 1.6 Stale comment claiming work-stealing is off — FIXED

`scheduler.rs:456`

```rust
// Work-stealing disabled for debugging.
// TODO: re-enable after fixing context corruption.
```

Stealing is not disabled — `try_steal_and_run` is called two lines below. The
context corruption it refers to was fixed in `4b8d7c2`. The comment now
misdescribes the code in a subsystem where people trust comments.

**Fixed:** replaced with what the backoff actually does.

---

## 2. Performance

### 2.1 `clock_gettime` reads the CMOS RTC on every call — FIXED

`syscalls/mod.rs:826`

```rust
let rtc = crate::drivers::rtc::read_rtc();
```

Every call does several 0x70/0x71 port round-trips. Under KVM each is a VM exit;
on real hardware the RTC is a genuinely slow device. It is also racy — nothing
handles the update-in-progress flag — and the returned struct is
`[hour, minute, second, 0, ...]`: no date, no epoch, no sub-second resolution.
`std::time::SystemTime` cannot be built on it, which is what `todo.txt` is
describing as "add system time (real time)".

**Fixed** exactly that way: `timer::init_wall_clock` samples the RTC once after
HPET init and pins it to the counter, and the syscall returns nanoseconds since
the Unix epoch. One point the entry missed: waiting for the update-in-progress
flag does not make an RTC read atomic, because the flag rises again shortly
before each update, so a read can straddle a carry. `read_rtc` now repeats until
two reads agree — a torn value would now be permanent rather than transient.
Verified against the host clock: guest 18:01:12 UTC vs host 18:01:38 with 26 s
of test runtime between them.

### 2.2 The kernel logs on the mmap and thread-exit hot paths — FIXED (b2b02f5)

`syscalls/memory.rs` has 20 `log!` sites, including one per successful `mmap`
(`:244 "mmap: lazy mapped at ..."`) and one per call at `:103`. `thread_exit`
logs a line per thread.

Logging is asynchronous — `log()` pushes to a queue drained by a kthread — but
the caller still pays a `String` allocation per event on the allocation hot
path, and the drain side writes to the UART a byte at a time under a global
lock, with a VM exit per byte. `threadtest` alone produced hundreds of lines a
second in every soak, and the serial lock saturating is what starved TLB
shootdowns before the `IrqSpinlock` fix.

**Fixed** by `log_debug!`, which reads a relaxed atomic before formatting, so a
disabled site costs one load and no allocation. Off unless the kernel command
line carries `loglevel=debug` — a dial rather than a rebuild. Failure paths
stayed on `log!`. Six threadtest+hammer iterations went from dozens of lines
each to zero; one threadtest with `loglevel=debug` still emits 37.

### 2.3 A heap allocation per path-taking syscall — FIXED

Seven sites do `vec![0u8; MAX_PATH_LEN]` with `MAX_PATH_LEN = 1024`
(`syscalls/io.rs:63`), one per `open`/`stat`/`mkdir`/`unlink`/`list_dir`/…

**Fixed** with `syscalls::copy_user_path`, which fills a caller-owned stack
array and returns a `&str`. It also collapses seven copies of the same
copy-validate-truncate block into one helper.

### 2.4 `pick_sched` is quadratic — FIXED

`thread/util.rs:112` calls `schedulers.iter().nth(idx)` inside a loop over all
`n` schedulers, so picking a CPU is O(n²) in CPU count. n is at most 128 and
typically ≤ 16, so this is small today and only worth fixing when the map is
touched anyway.

**Fixed** with `.iter().cycle().skip(start).take(n)`, taken while the map was
open for the affinity filter (1.2).

### 2.5 TCP retransmit clones whole segments — LOW

`net/tcp.rs:596` — `resends.push(seg.data.clone())` copies the full segment on
every resend. Under loss this allocates and memcpys per retry.

**Fix:** keep segments in `Arc<[u8]>` and clone the handle.

### 2.6 TLB shootdown is globally serialized — MEDIUM, by design

`memory/tlb.rs` funnels every shootdown through one `active` flag and one global
request slot, so unrelated `munmap`s on different CPUs in different address
spaces serialize against each other. That is a deliberate simplification (the
comment says so), and it is correct; it is also the obvious scaling wall once
core counts rise.

**Fix, when it matters:** per-CPU request slots, or skip the IPI entirely for
address spaces no other CPU has loaded — track a per-mm CPU mask and shoot down
only that set. The second is the bigger win and is the standard approach.

### 2.7 `find_free_address` always scans from the base — LOW

`memory/vma.rs:315` first-fits from `mmap_base` on every call, O(VMAs) per
`mmap`. The comment explains this is deliberate: a cursor would never wrap given
128 TiB of space above it, so freed ranges would never be reused. The tradeoff
is right; noting it because the cost grows with a process's VMA count and there
is a middle option (cursor plus an explicit wrap) if it ever shows up in a
profile.

---

## 3. Missing syscalls and interfaces

102 syscalls exist. The conspicuous absences, roughly by how much they block real
programs:

| Missing | Why it matters |
|---|---|
| `setuid` | `UserThreadInfo` carries `user_id`/`group_id`; only the getters exist |

**The CLOEXEC entry was rejected on its original premise and later became
real.** When this audit was written EDOS had no `exec`: `spawn` built a fresh
process and handed it exactly three descriptors, `fork` copied the whole table
(which is what fork is for and not what CLOEXEC governs), and there was no
`O_NONBLOCK` for `F_SETFL` to set. `FD_CLOEXEC` would have been a bit nothing
could observe. `execve` (59) is what gave it something to govern, and `fcntl`
(72) with `FD_CLOEXEC` shipped alongside it. `O_NONBLOCK` still does not exist,
so `F_SETFL` remains unimplemented on purpose.

`pread`/`pwrite` (audit-shipped, syscalls 17/18) and `getuid`/`getgid`
(102/104) were the two worth doing, and are in. `setuid` was deliberately **not**
added: with no permission model to enforce, a freely callable `setuid` is a
privilege change that lies about being one.

Also worth noting from `todo.txt`, still true: `edos_lib` duplicates `edos_rt`'s
syscall wrappers, and `SYS_OPEN` takes a NUL-terminated path while `SYS_STAT`
takes pointer+length, so `edos_rt` allocates a `CString` for every open.

---

## 4. Scheduler

Beyond affinity (1.2), things that look like the next real improvements:

- **Load is measured as `thread_count`.** A sleeping thread weighs the same as a
  CPU-bound one, so `pick_sched` and `try_rebalance` balance thread counts
  rather than load. A runnable-count or decayed-utilization metric would place
  threads far better for the same complexity.
- **No priority inheritance.** The starvation fix bounds how long a low-priority
  lock holder can be passed over, but a high-priority waiter still waits behind
  it. This is the classic reason to add PI to the blocking primitives, and the
  rank table already gives a place to hang it.
- **The timeslice is a flat 5ms** regardless of priority, so priority affects
  pick order but never share of CPU.
- **The idle loop polls for steals** on a backoff (`run_idle`) rather than being
  told. An IPI from a CPU that just enqueued work onto a long runqueue would cut
  steal latency and let idle CPUs stay halted.

---

## 5. Smells

- **204 `unwrap()`/`expect()` in kernel code.** Most are genuinely infallible
  but the density makes the real ones hard to find. Worth a pass that converts
  the ones on I/O and parsing results, and leaves a comment on the ones that are
  structurally impossible. The ELF loader is done: its field reads go through
  `le_u16`/`le_u32`/`le_u64`/`le_i64`, which bound-check.
- **`fs/` is 17.5k lines and `drivers/` 13.6k**, together over half the kernel's 61k.
  Nothing wrong, but both are past the size where a module-level README pays for
  itself.
- **Only 6 TODO markers in 48k lines**, which is genuinely good hygiene; one of
  them (1.6) was stale and is gone, leaving 5.

---

## What I checked and discarded

Recording these so the next pass does not re-litigate them:

- **ELF header `unwrap()`s** — the fixed-size slices were preceded by explicit
  length checks, so they could not fail. What the audit missed one layer down:
  the *relocation* walk panicked the kernel outright on a reloc kind or a
  malformed field, and any user can execute any file it can read. Both are
  `ElfLoadError` returns now, and the loader validates `e_ident` itself rather
  than trusting `sys_spawn`'s probe. `programs/exectest` test 6 is the
  regression test.
- **`PS2_LOCK` taken in IRQ context** (`drivers/mod.rs:32`) — both callers are
  interrupt handlers and x86 interrupt gates clear IF, so it cannot self-deadlock
  on one CPU; cross-CPU it is a few port reads. Correct as written and
  documented as such.
- **Device IRQ handlers touching driver state** — they only wake a kthread
  (`interrupts/io.rs:26-58`), which is what makes 1.3 a latency issue rather
  than a deadlock.
- **`run_idle` recursing into itself** — guarded by the idle/`has_work` check in
  `tick_prepare`.
