# Userspace Roadmap

77 programs and 2 libraries, all in the `programs/` cargo workspace.

## What exists

| Area | Programs |
|---|---|
| Init | `edos-init` (the only process the kernel starts; supervises the GUI session) |
| GUI | `edos-wm` (compositor, decorations, desktop menu), `edos-terminal`, `edos-taskbar` (panel + applications menu), `wintest` |
| Shell | `edos-sh` |
| Editor | `edos-vi` |
| Files | `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `rmdir`, `touch`, `stat`, `find`, `du`, `diff` |
| Text | `grep`, `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `tr`, `tee`, `hexdump`, `xargs` |
| Checksums | `sha256sum` |
| Inspection | `file` |
| System | `ps`, `free`, `uname`, `dmesg`, `df`, `mount`, `kill`, `sync`, `env`, `shutdown`, `strace` |
| Network | `ping`, `dns`, `http`, `wget`, `dnsprobe` |
| Audio | `play` |
| Misc | `echo`, `write`, `seq`, `yes`, `sleep`, `true`, `false`, `basename`, `dirname`, `cal`, `hello` |
| Stress tests | `alloctest`, `forktest`, `mmaptest`, `evicttest`, `lockordertest`, `inflighttest`, `threadtest`, `iotest`, `tcptest`, `exectest`, `killtest`, `vectest`, `sigtest` |
| Libraries | `edos_lib` (syscall wrappers), `edos_render` (fonts, text, icons, theme, widgets, windows) |

## Done

**Phase 1, shell conditionals and scripting staples.**
`true`, `false`, `kill`, `basename`, `dirname`, `cut`, `cal`.

**Phase 2, kernel stress-testers.**
`find` and `du` exercise readdir and stat at scale, `diff` is a classic LCS
implementation, `xargs` stress-tests spawn and enables composability.

**`sha256sum`** (Phase 3). Streaming SHA-256, so a file never has to fit in
memory; exercises large-file read through the page cache. Verified against the
NIST vectors and fuzzed against `hashlib` across 30 sizes, concentrating on the
block-boundary cases (55/56/57, 63/64/65, 8191/8192/8193).

**`file`** (Phase 3). Identifies a file from its leading 512 bytes: 20 magic
signatures with longest-match wins, ELF class/endianness/type decoding, and a
UTF-8 text heuristic. Classifications agree with GNU `file` on ELF binaries,
scripts, text, empty files and directories.

**`strace`** (Phase 4, the kernel-aware one that shipped first). A per-thread
trace mark checked at the syscall dispatch choke point, records into a ring the
tracer drains, and `/proc/syscalls` publishing the kernel's own syscall table so
the formatter carries no second copy of it. Follows children, shows what a
blocked process is blocked in, and captures both what a call was handed and what
it filled in. Written up in [`strace.md`](strace.md).

Candidates beyond these phases, ranked by the kernel path each would exercise,
are in [`PROGRAMS.md`](PROGRAMS.md).

## Phase 3: pure userspace, higher complexity

| Program | Why it matters | Notes |
|---|---|---|
| `tar` | archive create and extract; useful for host to guest transfer | exercises open/read/write/unlink/mkdir at scale |
| `top` | live system monitor in a terminal | needs terminal raw mode and a refresh loop. The graphical half shipped as `edos-procview`, and the procfs gap it found is closed: `/proc/processes` has an RSS column and `/proc/<tid>/status` carries `VM Size` and `Resident` |
| `snake` | terminal game on a timer | good demo of poll and time APIs, raw stdin |

## Phase 4: kernel-aware

| Program | Why it matters | Kernel gap |
|---|---|---|
| TCP echo server | validates the TCP state machine from userspace | none; the client side works now (`tcptest`), the listen/accept path is untested |
| `netstat` | listening and established sockets | needs a read path into `net/tcp.rs` `CONNECTIONS`, as `SYS_NETSTAT` or `/proc/net/tcp` |
| BMP image viewer | a real GUI app over window syscalls and shared memory | none; exercises the compositor |
| `nproc` | CPU count | needs `SYS_NPROC` or `/proc/cpuinfo` |

## `edos_render` is the shared surface

Every graphical program links it, so a change here reaches the compositor, the
panel, the terminal and every widget at once. What lives in it:

| Module | What it owns |
|---|---|
| `font` | Outline faces loaded from `/share/fonts` through `fontdue`, and the glyph cache. Lato for chrome, JetBrains Mono for character grids. Falls back to the built-in bitmap face when a file is missing, so a bad install costs type rather than the session. |
| `text` | The one blitter and the one measurement path, so the window manager, the panel and the widgets agree on where a glyph sits and how wide a string is. |
| `icons` | 16x16 monochrome masks, tinted at draw time so an icon takes the colour of its state. Hand-authored: a desktop with eight icons does not need a theme, a lookup path and a cache, each of which can be missing at boot. |
| `image` | A BMP decoder (24- and 32-bit, uncompressed) and a bilinear scale-to-cover. BMP because it is the one raster format a machine can write without a library and this OS can read without one. Used for wallpapers; an image viewer would use the same two functions. |
| `metrics` | One spacing scale derived from a single unit, and the shared control height. |
| `theme` | Every colour in the shell. |
| `widgets` | Controls, layout, and the terminal grid. |
| `window` | Window syscalls, and the `WindowListEntry` ABI. |
| `graphics` | Framebuffer, textures, `Screen`. |

Two things that break silently rather than loudly:

- **`WindowListEntry` mirrors the kernel's struct** in `kernel/src/syscalls/window.rs`
  field for field. Change one without the other and the compositor reads garbage;
  nothing catches it at compile time.
- **Text is measured, not counted.** The chrome face is proportional, so
  `chars().count() * char_width()` is wrong everywhere except a mono grid. Use
  `widgets::text_width`.

## The std fork caught up with the syscall table

Every program here is ordinary `std` Rust, so a syscall the fork does not know
about is a syscall no program can use without dropping to `edos_lib`. Nineteen
syscalls had landed without the fork moving; `edos_rt` 0.0.42 and the shims in
`library/std/src/sys/*/edos.rs` on `edos_std_v2` close that gap:

| std API | Was | Now |
|---|---|---|
| `fs::symlink`, `fs::read_link` | `unsupported()` | `SYS_SYMLINK` 88, `SYS_READLINK` 89 |
| `fs::set_times`, `File::set_times` | `unsupported()` | `SYS_UTIMENSAT` 280, the second through `futimens` |
| `FileAttr::modified`/`accessed`/`created` | `unsupported()` | the times `SYS_STAT` already carried |
| `FileType::is_symlink` | always `false` | the kind a directory listing reports |
| `File::read_vectored`/`write_vectored` | `unsupported()`, `is_*_vectored() == false` | `SYS_READV` 19, `SYS_WRITEV` 20 |
| `thread::sleep` | rounded to `SYS_SLEEP_MS`, so sub-ms sleeps were 0 or 1 ms | `SYS_NANOSLEEP` 35 |
| `ReadDir` | one `SYS_LIST_DIR` call sized to the whole directory | `SYS_GETDENTS` 78, a chunk at a time |
| `Path::try_exists` | a full `stat` | `SYS_ACCESS` 21 with `F_OK` |
| `OpenOptions::open` | allocated a `CString` per open | `SYS_OPENAT` 257 takes pointer+length |

`futimens` is new kernel work rather than a wrapper: `SYS_UTIMENSAT` took a path
and a `File` has only a descriptor, so a null path now means "the file this
descriptor names", which is the POSIX form. `iotest` test 9 covers it.

Three stay unsupported on purpose: `fs::hard_link` (the kernel has no hard
links), `set_times_nofollow` (`utimensat` rejects `AT_SYMLINK_NOFOLLOW`, which
`set_times` cannot honour), and the `File::lock` family (no advisory locking).
`lstat` is `stat`, because nothing below it can decline to follow a link;
`readlink` is what reports on the link itself.

The publish loop this needs is in the README, and a `0.0.z` requirement is
exact, so the fork's pin has to move in the same pass.

## Kernel gaps this roadmap exposes

These are the gaps *userspace programs* expose. For kernel-internal work —
correctness, locking, perf hot spots and missing syscalls — see
[`AUDIT.md`](AUDIT.md), with the prioritised list in `ideas.txt`.

1. **procfs depth.** CPU time, RSS and virtual size are all there now. What a
   `top` still lacks is a *rate*: every counter is cumulative, so the reader has
   to difference two samples itself to get CPU percent.
2. **TCP introspection.** `/proc/net` carries interface state (link, address,
   gateway, resolver); `netstat` still needs a read path for the connection
   table in `net/tcp.rs`.
3. **Accept backlog.** A concurrent server will test SYN backlog behaviour.
4. **Large-file I/O.** `sha256sum` and `tar` push files larger than RAM through the
   block and page caches.
5. **Rapid spawn and exit.** `xargs` in loop mode hammers the reaper and zombie
   collection.
6. **CPU count.** Not exposed to userspace at all today.

## Signals: what landed, and what is left

Five kernel items shipped 2026-08-11 (`programs/sigtest` covers all of them);
the shell half has not been written yet.

**Done:**

1. **Process groups.** `Thread::pgid` with `setpgid`/`getpgid`, and
   `Pty.foreground_pgid` in place of a single pid, so Ctrl+C reaches a whole
   pipeline. `kill` takes the POSIX forms: a positive pid is one process, 0 is
   the caller's group, a negative pid is that group. `tcsetpgrp`/`tcgetpgrp`
   hand the terminal to a group, which is what `fg` will need.
2. **Stop and continue.** `SIGSTOP`, `SIGTSTP` and `SIGCONT`, with Ctrl+Z
   wired into the line discipline. A thread suspends at the same boundary
   `killed` uses — a syscall return or a tick out of ring 3 — because that is
   where it provably holds nothing, so a suspended process never sits on a
   filesystem lock. `waitpid` gained `WAIT_UNTRACED` and reports a stopped
   child distinguishably; `/proc` shows `Stopped` and a `PGID` column.
3. **Userspace signal handlers.** `sigaction` takes a real function address and
   a restorer; delivery builds a frame on the user stack at the syscall-return
   boundary and `sigreturn` unwinds it, restoring the full context including
   the interrupted syscall's return value. **A thread that never syscalls never
   runs a handler** — default actions still reach it from the timer tick, so
   Ctrl+C kills a spinning process but cannot be caught by one.
4. **`SIGPIPE`.** A write to a pipe with no reader used to buffer into the
   kernel heap forever; it now raises `SIGPIPE` and returns `EPIPE`.
5. **`SIGCHLD`** is sent to the creator when a child exits.

6. **Shell job control.** `JobStatus` has `Stopped`, a job carries the pid of
   every stage plus the process group they share, and `fg`/`bg` resume one.
   `spawn_pipeline` puts the first stage in a group of its own and the rest
   into it, so one Ctrl+C reaches a whole pipeline; the shell hands the
   terminal over with `tcsetpgrp` and takes it back after every job,
   background ones included. Still open: `fg` resumes a job but leaves it
   marked stopped in `/proc` (see WORKING-NOTES).

**Done:**

- fd-numbered redirection. `Redirects` in `programs/edos-sh/src/command.rs` is
  an ordered list of operations, so `2>file`, `2>&1`, `1>&2` and `&>file` work,
  in a pipeline stage as well as on a plain command. Only descriptors 0, 1 and
  2 can be named, since those are the three `SYS_SPAWN2` gives a child.
- Pathname expansion. `programs/edos-sh/src/glob.rs` matches `*`, `?` and
  `[...]` one path component at a time over `readdir`, for command arguments
  and for a `for` loop's word list. A pattern matching nothing is passed
  through unchanged, a quoted or backslash-escaped word is never a pattern, and
  `*` does not pick up dotfiles. The command word itself is not expanded.

## Next: time is UTC everywhere

Nothing in the system has a timezone concept. The kernel reads the RTC as UTC
(`timer.rs`), `edos_lib::time::ClockTime::from_unix_nanos` documents itself as
UTC, and the panel clock formats that directly — so the desktop clock is wrong
by the local offset for everyone not on UTC. There is also no `date` program, so
the panel is the only way to read the time at all. Wants: an offset the session
can carry (an environment variable is enough; `/etc` does not exist yet), one
place in `edos_lib` that applies it, and `date`.
