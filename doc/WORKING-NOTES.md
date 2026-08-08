# Working notes, session of 2026-08-08

State of the tree, what changed, and what is still open. Written for whoever
picks this up next, which will usually be an agent with no memory of the
session.

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
  failed. Fixed in the fork as commit `b7af81795f6`, **committed locally in
  `~/dev/rust` but not pushed**, so it exists on this machine only.
- `sha256sum` and `file`, two Phase 3 userspace programs.

`mmaptest` went from failing at test 1 to all 10 passing on both `/var` and
`/tmp`.

---

## The open bug that matters

**A window-registry reader got stuck and wedged the whole GUI**, with all four
CPUs spinning on `WINDOW_REGISTRY.write()`. Full writeup in
[`bugs/2026-08-08-window-registry-stuck-reader.md`](bugs/2026-08-08-window-registry-stuck-reader.md).

It is **not root-caused.** What is established:

- The lock word read `0x4`, which is one reader and no writer, so a reader never
  released rather than a lock cycle.
- Nothing was killed. No panics, no faults, no non-zero exits, and the last
  process exit was 219s before the hang. That rules out the obvious "no
  unwinding, so a killed thread leaks its guard" theory.
- Therefore the holder was **parked** while holding the guard, and was not
  running on any CPU, which is why it never appeared in the register dump.

That points at the park/wake machinery rather than at the window code.
`bugs/2026-04-13-sched-park-wake-missed-wakeup.md` describes the same failure
shape from a lost wake, and the owner considers that code old and suspect.

**A 3000-round soak did not reproduce it**, but that run contained the
`sys_window_list` fix, so it cannot separate "fixed" from "not triggered".
Reverting that fix and soaking again would separate them.

To catch it next time, rebuild with the instrumentation and read the table:

```bash
make edos-x86_64.iso CARGO_FLAGS="--features window-lock-debug"
scripts/edos-vm start
scripts/window-lock-soak 3000
```

Slots decode as `(tid << 8) | site` and name the thread that never released.
`WINDOW_REGISTRY_READER_ACQUIRES` is the positive control: live slots last
microseconds, so an empty table only means something if that counter is moving.
It reads about 259/sec on an idle desktop.

---

## Cross-repo, deliberately not done

Both need a decision rather than a drive-by, because they leave this repo.

1. **The userspace allocator fragments without bound.** This is what looked like
   an `edos-wm` heap leak: 64 KiB every 9.2s forever on an idle desktop. It is
   `edos_rt`'s `PoolAllocator`, which never coalesces adjacent free blocks, drops
   blocks smaller than `FreeBlock`, and loses the tail of exactly-fitting blocks.
   Growth tracks allocation *rate*, not retention, which is why the period is so
   exact. Every long-running program is affected. Fixing it means publishing to
   crates.io, which is irreversible.
2. **The std fork fix above is committed but unpushed** (`b7af81795f6` on
   `edos_std_v2`). Push it, or the next person who rebuilds the toolchain from
   a fresh clone loses the fix and mmaptest regresses to failing at test 3.
   `edos_rt` still has no `RDONLY`/`WRONLY`/`RDWR`/`TRUNCATE` constants, so std
   spells the values out itself; moving them into `edos_rt` is cleaner and needs
   a release.

Also open, lower priority: `decode_error_kind` in the std fork maps only five
errnos, so everything else displays as "uncategorized error", and the AHCI
watchdog `restarting` gate, which is a latency issue rather than a lost I/O and
should land with runtime validation because it touches the storage submit path.

---

## Things that will bite you

- `make edos-x86_64.iso` re-invokes the kernel target **without** any
  `CARGO_FLAGS` you passed earlier, silently replacing an instrumented build
  with a plain one. Pass the flags to the ISO target itself.
- `cargo` does not notice that `std` changed. After rebuilding the toolchain,
  `cargo +edos clean` in `programs/` or you will keep linking the old one, and
  the build will cheerfully report success.
- `sg` is also the name of the `ast-grep` binary. Scripts that need the group
  tool must use `/usr/bin/sg`.
- Symbol addresses move on every kernel rebuild, so resolve them from
  `kernel/kernel` at runtime rather than hard-coding them.
