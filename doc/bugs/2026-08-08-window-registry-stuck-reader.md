# Window registry stuck reader: whole-GUI deadlock

**Status:** partially diagnosed. One confirmed defect fixed (see below); the
root cause of the observed hang is **not** established, and the hang has not
been reproduced. Observed once, after ~630s of continuous synthetic clicks and
keystrokes through `scripts/edos-vm`.

A 3000-round instrumented soak afterwards (`scripts/window-lock-soak`, ~45
minutes, 207k registry acquisitions) did not reproduce it, and the reader table
stayed empty throughout. That run included the `sys_window_list` fix below, so
it cannot distinguish "the fix removed the cause" from "the workload does not
hit the trigger". Reproducing on a build with the fix reverted would separate
the two.

---

## Symptoms

The desktop freezes completely and never recovers:

- Taskbar clock stops advancing.
- Pointer frozen; further mouse motion and keystrokes have no effect.
- Serial log goes permanently silent (last line was a routine `edos-wm` mmap).
- One host vCPU thread pinned at 99.9%, the other three idle.
- The guest is *not* panicked. `query-status` still reports `running`.

Every vCPU is spinning in `spin::RwLock<WindowRegistry>::write`:

| CPU | RIP | Site |
|---|---|---|
| 0, 1, 3 | `0xffffffff800227cd` | `sys_window_set` (`syscalls/window.rs:128`) |
| 0 (later sample) | `0xffffffff80022feb` | `sys_window_list` (`syscalls/window.rs:366`) |
| 2 | `0xffffffff80126d6b` | `handle_mouse_event` (`window/input.rs:332`) |

---

## Root cause

Reading the lock word out of the hung guest is decisive:

```
x /1gx 0xffffffff801a5c40    # WINDOW_REGISTRY
ffffffff801a5c40: 0x0000000000000004
```

`spin` encodes `READER = 1 << 2`, `UPGRADED = 1 << 1`, `WRITER = 1`. The value
`0x4` means **one outstanding reader and no writer**. This is not a writer
deadlock. A single read guard leaked, and since a writer can never acquire while
any reader is outstanding, every subsequent `write()` spins forever.

`WINDOW_EVENTS` at `0xffffffff801a8f40` reads `0x0`, so it is not involved.

### What the evidence rules out

The first hypothesis was a guard leaked by a killed thread: EDOS is `no_std` with
no unwinding, so a thread killed while holding a guard never runs
`RwLockReadGuard::drop`. **The serial log disproves this.** Across the whole run
there are no panics, no page faults, and no non-zero exits; all ten recorded
exits are `code=0`, and the last one is at t=410s, 219 seconds before the hang at
t=629s. Nothing died.

### What remains

The reader count of 1 is therefore most likely *legitimate*: a thread acquired
the read guard and then stopped making progress without releasing it. Since all
four CPUs were spinning on `write()`, the holder was not running on any of them,
so it was parked or otherwise descheduled while holding the guard.

That points at the park/wake machinery rather than at the window code
specifically. Compare `doc/bugs/2026-04-13-sched-park-wake-missed-wakeup.md`: a
lost wake leaves a thread parked forever, and if that thread holds a spin read
guard, every writer then spins forever behind it.

**The specific holder is not yet identified.** Naming it needs instrumentation
that records the tid and site of each live reader, readable from outside the
guest, so the next occurrence names the culprit instead of inviting another
guess.

### Confirmed defect, fixed

Independent of the above, `sys_window_list` held the read guard across
`try_copy_to_user` for every visible window. A user copy can demand-fault, and a
ring-0 demand fault runs with interrupts enabled and can park on a page fill.
Holding a spin lock across a park makes every other CPU spin for the duration of
disk I/O, which matches the observed "all four CPUs spinning" shape exactly.

Fixed by snapshotting the entries into an owned `Vec` under the guard, dropping
the guard, then copying out. This is a real defect on its own merits; whether it
is *the* cause of this hang is unproven.

---

## Reasoning rules going forward

- **Never hold a spin lock across a user-memory access.** A user copy can
  demand-fault, and a demand fault can park. Snapshot into a kernel-owned buffer,
  drop the guard, then copy out. This is the rule the FS layer already follows
  for `inode.lock` and disk I/O, applied to userspace copies.
- **A parked thread holding a spin read guard stops the whole machine.** Unlike a
  blocking mutex, every other CPU busy-waits behind it. Anything that can park
  must not be reachable with a spin guard live.
- **A guard held across a killable operation would also leak**, because there is
  no unwinding: the reaper frees the stack without running destructors. That did
  not happen here, but the hazard is real and worth avoiding for the same reason.
- **A `spin::RwLock` reader leak is strictly worse than a mutex leak**, because
  it is invisible until the first writer arrives, and then it takes down every
  CPU at once rather than one caller.
- **Unranked does not mean harmless.** `WINDOW_REGISTRY` was excluded from the
  rank table (`doc/invariants/lock-order.md`) on the grounds that window
  registries are leaf locks outside the fs/mm hot paths. That is true and it did
  not help: the failure was hold-duration and kill-safety, not acquisition order.

---

## Remaining work

1. **Instrument the holder.** Record tid and acquisition site for every live
   reader in a debug-only static so the next occurrence names the thread.
   Without this, any further root-cause claim is a guess.
2. **Audit the other guard sites.** `sys_window_set` (`:128`) holds the *write*
   guard across comparable work; `syscalls/window.rs:205, 250, 424` and
   `window/input.rs:300, 413` hold read guards across `send_event`.
3. **Audit park/wake compliance** in the window and graphics paths against the
   contract in `doc/bugs/2026-04-13-sched-park-wake-missed-wakeup.md`:
   `thread_park_while` may return spuriously, so every caller must loop.
   `window/input.rs:286` does loop, and is fine.

Ranking `WINDOW_REGISTRY` would not have caught this; the failure is hold
duration, not acquisition order.

---

## If this reappears

1. `scripts/edos-vm qmp human-monitor-command '{"command-line":"info registers -a"}'`
   and symbolize each RIP with `addr2line -e kernel/target/x86_64-unknown-none/debug/edos-kernel -f -C -i`.
2. If the RIPs land in `spin::rwlock`, read the lock word for the contended lock.
   `nm -C` the kernel for the symbol, then `x /1gx <addr>`.
3. Decode: `& 1` is a live writer, `>> 2` is the reader count. A non-zero reader
   count with no writer, and no forward progress, means a stuck reader rather
   than a lock cycle: look for a holder that parked, or died, with the guard live.
4. Check the serial log for kills and non-zero exits before assuming a kill leak.
   Their absence means the holder is parked, not dead.

---

## Saved artifacts

`logs/2026-08-08-window-registry-hang/` (gitignored): serial log, all four
register dumps, screenshot at the hang, and working notes.
