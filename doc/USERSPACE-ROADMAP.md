# EDOS Userspace Roadmap

## Existing Programs (47)

**GUI system:** edos-wm, edos-terminal, edos-taskbar, wintest, edos_render (lib)
**Shell:** edos-sh
**File utils:** cat, cp, mv, rm, rmdir, mkdir, touch, stat, ls
**Text processing:** grep, head, tail, wc, sort, uniq, tr, tee, hexdump
**System info:** ps, free, uname, dmesg, df, mount
**Network clients:** ping, dns, http, wget, sync
**Audio:** play
**Utilities:** seq, yes, sleep
**Editor:** edos-vi
**Misc:** echo, write, env
**Tests:** alloctest, forktest, mmaptest, evicttest, lockordertest, inflighttest
**Library:** edos_lib

## Phase 1 — Quick wins, no kernel changes (DONE)

| Program | Why it matters |
|---|---|
| `true` | Exit code 0. Required for shell conditionals. |
| `false` | Exit code 1. Required for shell conditionals. |
| `kill` | Send signals to processes. Uses existing `SYS_KILL`. |
| `basename` | Strip directory prefix from path. Shell scripting staple. |
| `dirname` | Strip last component from path. Shell scripting staple. |
| `cut` | Select columns from delimited text. Complements text-processing toolkit. |
| `cal` | Print calendar. Shows off date/time, pure userland. |

## Phase 2 — Kernel stress-testers (no kernel changes)

| Program | Why it matters |
|---|---|
| `find` | Recursive file search. Exercises readdir/stat at scale. |
| `du` | Disk usage. Walks trees, calls stat. Small codebase, real use. |
| `diff` | File comparison. Fundamental dev tool. Classic LCS algorithm. |
| `xargs` | Build commands from stdin. Enables composability. Stress-tests spawn. |

## Phase 3 — Higher complexity, still pure userspace

| Program | Why it matters | Notes |
|---|---|---|
| `tar` | Archive creation/extraction. Useful for host↔guest file transfer. | Exercises create/open/read/write/close/unlink/mkdir at scale. |
| `top` | Real-time system monitor. Requires terminal raw mode + refresh loop. | Will surface gaps in what procfs provides (per-process CPU time, memory RSS). |
| `snake` | Terminal game with timer. Good demo of poll/time APIs. | Uses raw stdin mode. |
| `file` | Detect file type by magic bytes. Standard Unix tool. | Pure userspace pattern matching. |
| `hash` (sha256sum) | Cryptographic checksums. Exercises large-file read. | Pure Rust impl, no crates needed. |

## Phase 4 — Kernel-aware programs

| Program | Why it matters | Kernel gap |
|---|---|---|
| **TCP echo server** | Tests accept/listen/poll on sockets. Validates TCP state machine from userspace. | None (syscalls already exist). May surface accept backlog or poll wakeup bugs. |
| **`netstat`** | Show listening/established sockets, TCP connection table. | Need `SYS_NETSTAT` or `/proc/net/tcp`. Kernel already tracks everything in `net/tcp.rs` CONNECTIONS; just needs a read path. Low effort, high payoff. |
| **Image viewer** (BMP) | GUI app using window syscalls + shared memory. Tests render/compositor from a real app. | None (window syscalls exist). |
| **`nproc`** | Print number of CPUs. Simple, exposes CPU count. | Currently no way to query from userspace. Trivial kernel addition. |

## Known Kernel Gaps Found During Roadmap

1. **procfs depth** — `top` needs per-process CPU time, memory RSS. Check if procfs entries provide this.
2. **TCP connection introspection** — `netstat` needs a read path for the TCP connection table.
3. **TCP accept backlog** — A server with concurrent connections will test SYN backlog queue behavior.
4. **Large-file I/O** — `hash` (sha256sum) and `tar` stress the block/page cache with files > RAM.
5. **Rapid spawn/exit** — `xargs` in loop mode spawns many short-lived processes. Tests reaper, zombie collection.
6. **CPU count** — `nproc` needs `SYS_NPROC` or `/proc/cpuinfo`. Currently not exposed.
