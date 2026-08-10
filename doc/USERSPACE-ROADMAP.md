# Userspace Roadmap

75 programs and 2 libraries, all in the `programs/` cargo workspace.

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
| System | `ps`, `free`, `uname`, `dmesg`, `df`, `mount`, `kill`, `sync`, `env`, `shutdown` |
| Network | `ping`, `dns`, `http`, `wget`, `dnsprobe` |
| Audio | `play` |
| Misc | `echo`, `write`, `seq`, `yes`, `sleep`, `true`, `false`, `basename`, `dirname`, `cal`, `hello` |
| Stress tests | `alloctest`, `forktest`, `mmaptest`, `evicttest`, `lockordertest`, `inflighttest`, `threadtest`, `iotest`, `tcptest`, `exectest`, `killtest`, `vectest` |
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

## Phase 3: pure userspace, higher complexity

| Program | Why it matters | Notes |
|---|---|---|
| `tar` | archive create and extract; useful for host to guest transfer | exercises open/read/write/unlink/mkdir at scale |
| `top` | live system monitor | needs terminal raw mode and a refresh loop; will surface procfs gaps |
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

## The std fork lags the syscall table

Every program here is ordinary `std` Rust, so a syscall the fork does not know
about is a syscall no program can use without dropping to `edos_lib`. Nineteen
syscalls landed without the fork moving, and these std APIs are `unsupported()`
or emulated as a result (`library/std/src/sys/*/edos.rs` in the `edos_std_v2`
branch):

| std API | Stub today | Syscall that closes it |
|---|---|---|
| `fs::symlink`, `fs::read_link` | `unsupported()` | `SYS_SYMLINK` 88, `SYS_READLINK` 89 |
| `fs::set_times`, `File::set_times` | `unsupported()` | `SYS_UTIMENSAT` 280 |
| `FileAttr::modified`/`accessed` | `unsupported()` | none; `SYS_STAT` already carries the times |
| `File::read_vectored`/`write_vectored` | `unsupported()`, `is_*_vectored() == false` | `SYS_READV` 19, `SYS_WRITEV` 20 |
| `thread::sleep` | rounds to `SYS_SLEEP_MS`, so sub-ms sleeps are 0 or 1 ms | `SYS_NANOSLEEP` 35 |
| `ReadDir` | one `SYS_LIST_DIR` call sized to the whole directory | `SYS_GETDENTS` 78 streams it |
| `Path::try_exists` | a full `stat` | `SYS_ACCESS` 21 with `F_OK` |
| `OpenOptions::open` | allocates a `CString` per open | `SYS_OPENAT` 257 takes pointer+length |

Three stay unsupported on purpose: `fs::hard_link` (the kernel has no hard
links), `set_times_nofollow` (`utimensat` rejects `AT_SYMLINK_NOFOLLOW`, which
`set_times` cannot honour), and the `File::lock` family (no advisory locking).

The work is mechanical and already prototyped: `programs/edos_lib` has a tested
wrapper for each of these, and `programs/iotest` covers them. Porting means
moving the wrapper into `edos_rt`, publishing it, and unstubbing the std shim.
See the `edos_rt` publish loop in the README; a `0.0.z` requirement is exact, so
the fork's pin has to move in the same pass.

## Kernel gaps this roadmap exposes

These are the gaps *userspace programs* expose. For kernel-internal work —
correctness, locking, perf hot spots and missing syscalls — see
[`AUDIT.md`](AUDIT.md), with the prioritised list in `ideas.txt`.

1. **procfs depth.** `top` wants per-process CPU time and RSS.
2. **TCP introspection.** `netstat` needs a read path for the connection table.
3. **Accept backlog.** A concurrent server will test SYN backlog behaviour.
4. **Large-file I/O.** `sha256sum` and `tar` push files larger than RAM through the
   block and page caches.
5. **Rapid spawn and exit.** `xargs` in loop mode hammers the reaper and zombie
   collection.
6. **CPU count.** Not exposed to userspace at all today.
