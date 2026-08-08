# Userspace Roadmap

60 programs and 2 libraries, all in the `programs/` cargo workspace.

## What exists

| Area | Programs |
|---|---|
| GUI | `edos-wm`, `edos-terminal`, `edos-taskbar`, `wintest` |
| Shell | `edos-sh` |
| Editor | `edos-vi` |
| Files | `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `rmdir`, `touch`, `stat`, `find`, `du`, `diff` |
| Text | `grep`, `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `tr`, `tee`, `hexdump`, `xargs` |
| System | `ps`, `free`, `uname`, `dmesg`, `df`, `mount`, `kill`, `sync`, `env` |
| Network | `ping`, `dns`, `http`, `wget` |
| Audio | `play` |
| Misc | `echo`, `write`, `seq`, `yes`, `sleep`, `true`, `false`, `basename`, `dirname`, `cal` |
| Stress tests | `alloctest`, `forktest`, `mmaptest`, `evicttest`, `lockordertest`, `inflighttest` |
| Libraries | `edos_lib` (syscall wrappers), `edos_render` (textures, widgets, windows) |

## Done

**Phase 1, shell conditionals and scripting staples.**
`true`, `false`, `kill`, `basename`, `dirname`, `cut`, `cal`.

**Phase 2, kernel stress-testers.**
`find` and `du` exercise readdir and stat at scale, `diff` is a classic LCS
implementation, `xargs` stress-tests spawn and enables composability.

## Phase 3: pure userspace, higher complexity

| Program | Why it matters | Notes |
|---|---|---|
| `tar` | archive create and extract; useful for host to guest transfer | exercises open/read/write/unlink/mkdir at scale |
| `top` | live system monitor | needs terminal raw mode and a refresh loop; will surface procfs gaps |
| `file` | detect type by magic bytes | pure pattern matching |
| `sha256sum` | checksums over large files | pure Rust, exercises large-file read |
| `snake` | terminal game on a timer | good demo of poll and time APIs, raw stdin |

## Phase 4: kernel-aware

| Program | Why it matters | Kernel gap |
|---|---|---|
| TCP echo server | validates the TCP state machine from userspace | none; may surface accept-backlog or poll-wakeup bugs |
| `netstat` | listening and established sockets | needs a read path into `net/tcp.rs` `CONNECTIONS`, as `SYS_NETSTAT` or `/proc/net/tcp` |
| BMP image viewer | a real GUI app over window syscalls and shared memory | none; exercises the compositor |
| `nproc` | CPU count | needs `SYS_NPROC` or `/proc/cpuinfo` |

## Kernel gaps this roadmap exposes

1. **procfs depth.** `top` wants per-process CPU time and RSS.
2. **TCP introspection.** `netstat` needs a read path for the connection table.
3. **Accept backlog.** A concurrent server will test SYN backlog behaviour.
4. **Large-file I/O.** `sha256sum` and `tar` push files larger than RAM through the
   block and page caches.
5. **Rapid spawn and exit.** `xargs` in loop mode hammers the reaper and zombie
   collection.
6. **CPU count.** Not exposed to userspace at all today.
