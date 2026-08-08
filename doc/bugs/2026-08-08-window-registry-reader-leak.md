# Window registry reader-guard leak: whole-GUI deadlock

**Status:** root-caused, not yet fixed. Found by an agent-driven soak, not by
hand: ~630s of continuous synthetic clicks and keystrokes through
`scripts/edos-vm`. Manual use is unlikely to reach it.

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

The leak is structural. `sys_window_list` takes the read guard at
`syscalls/window.rs:315` and still holds it at line 356, where it calls
`try_copy_to_user` in a loop over every visible window. That copy can fault:
ring-0 demand faults run with interrupts enabled, so the thread can park on a
page fill, and it can be killed there.

**EDOS is `no_std` with no unwinding.** When a thread is killed, the reaper frees
its stack; it does not run destructors for live locals. A `RwLockReadGuard` held
at the kill point is therefore never dropped, and the reader count is never
decremented. The registry is poisoned for the remaining uptime of the machine.

`edos-wm` calls `sys_window_list` every frame, so the exposure window is hit
continuously, which is why sustained input eventually lands a kill inside it.

---

## Reasoning rules going forward

- **A lock guard held across a killable operation leaks on kill.** With no
  unwinding, "the guard drops on early return" is only true for *returns*. It is
  false for kills. Any guard live across `try_copy_to_user`, a demand fault, a
  park, or anything that can take a signal is a permanent leak waiting to happen.
- **Never hold a lock across a user-memory access.** Copy into a kernel-owned
  buffer, drop the guard, then copy out. This is the same rule the FS layer
  already follows for `inode.lock` and disk I/O, applied to userspace copies.
- **A `spin::RwLock` reader leak is strictly worse than a mutex leak**, because
  it is invisible until the first writer arrives, and then it takes down every
  CPU at once rather than one caller.
- **Unranked does not mean harmless.** `WINDOW_REGISTRY` was excluded from the
  rank table (`doc/invariants/lock-order.md`) on the grounds that window
  registries are leaf locks outside the fs/mm hot paths. That is true and it did
  not help: the failure was hold-duration and kill-safety, not acquisition order.

---

## Fix

Build the `WindowListEntry` values into an owned `Vec` while holding the guard,
drop the guard, then copy to userspace. The same audit is needed at every read
site that touches user memory (`syscalls/window.rs:205, 250, 315, 424`) and at
`sys_window_set` (`:128`), which holds the *write* guard across similar work.

A rank for `WINDOW_REGISTRY` and `WINDOW_EVENTS` is worth adding afterwards, so
the tracker enforces the ordering between them, but ranking alone would not have
caught this.

---

## If this reappears

1. `scripts/edos-vm qmp human-monitor-command '{"command-line":"info registers -a"}'`
   and symbolize each RIP with `addr2line -e kernel/target/x86_64-unknown-none/debug/edos-kernel -f -C -i`.
2. If the RIPs land in `spin::rwlock`, read the lock word for the contended lock.
   `nm -C` the kernel for the symbol, then `x /1gx <addr>`.
3. Decode: `& 1` is a live writer, `>> 2` is the reader count. A non-zero reader
   count with no writer, and no forward progress, means a leaked read guard, so
   look for a guard held across a kill point rather than for a lock cycle.

---

## Saved artifacts

`logs/2026-08-08-window-registry-hang/` (gitignored): serial log, all four
register dumps, screenshot at the hang, and working notes.
