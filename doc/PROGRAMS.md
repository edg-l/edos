# Programs worth writing

Candidates for `programs/`, ranked by what each would *prove* rather than by how
useful it sounds. That filter is how the recent ones earned their place:
`edos-procview` found that procfs reported no per-process memory, `iotest` found
that absolute symbolic links resolved in the wrong namespace, and `strace`
turned every "it printed nothing" into a transcript ([`strace.md`](strace.md)).
A program that exercises a kernel path nothing else touches pays for itself the
first time it runs.

What already exists is inventoried in [`USERSPACE-ROADMAP.md`](USERSPACE-ROADMAP.md);
this file is what is missing and why it would be worth having. Entries leave
this file when they ship.

This file ranks by what a program would prove; it is not the build order. That
lives in Phase 5 of [`USERSPACE-ROADMAP.md`](USERSPACE-ROADMAP.md), ranked by
what someone else's first hour with the ISO runs into. The two agreed on the
file manager, which was the first entry here and has shipped as `edos-files`,
and disagreed on the settings panel, which Phase 5 split: runtime keyboard
layout went first on its own and has shipped as `keymap`, ahead of the panel
that would house it.

## The one to write next

**A graphical editor.** What is left of "a desktop that is not a terminal
launcher": `edos-files` can now reach a file without a shell but nothing
graphical can change one, so every edit is still `edos-vi` in a PTY. It is also
the third consumer of `widgets::text_input`, and the second one is what found
the two defects that had been in that widget since it was written, so a third
is worth having for the same reason.

## GUI

| Program | What it exercises that nothing does today |
|---|---|
| System monitor with sparklines | The *rate* gap: every procfs counter is cumulative, so plotting forces differencing. Would pull on `/proc/ahci_stats`, `/proc/block_cache` and per-CPU load |
| Paint | Damage rectangles and shm compositing harder than anything else: small, frequent, scattered dirty rects, the opposite of the window-drag case already measured in `WORKING-NOTES.md` |
| Minesweeper | Right-click. Nothing in the system uses a non-left mouse button, so that path is entirely untested |
| Settings panel | A graphical home for the settings that exist but have only a CLI and a file: the keyboard layout `keymap` writes, the wallpaper the desktop menu cycles. Would also be the first thing to want a runtime theme, which is still a `const` |
| Music player | HDA plus timing under a GUI. `play` proves the codec works; a seek bar proves the DMA ring survives being poked at |
| Disk-usage treemap | Deep recursion through the page cache, and genuinely useful on a 5G development root |
| Calendar popup | Small and concrete: `Action::Clock => {}` in `programs/edos-taskbar/src/main.rs` means the panel clock hovers, is clickable, and does nothing |

## CLI

### Would surface a kernel gap, which is the point

| Program | The gap |
|---|---|
| in-EDOS `fsck` | `efs-fsck` is host-side only, so the guest cannot check its own filesystem. This is the difference between an OS that can be repaired and one that has to be re-imaged |
| `ssh` (client) | `sshd` serves, but nothing here can reach out. Needs terminal raw mode on the local side and a `known_hosts` the user actually reads, neither of which the server side needed |
| `svc` | Nothing can start, stop or restart a service at runtime, because `edos-init` supervises a hardcoded array and has no control channel. It exposes the missing piece rather than the missing program: there are no named FIFOs, which is what every established supervisor is controlled through. Design and the rejected alternatives are in [`USERSPACE-ROADMAP.md`](USERSPACE-ROADMAP.md) |

**An SFTP subsystem in `sshd`** rather than a program of its own, and the
reason it belongs on this list: it is what turns "I can log in" into "I can work
on the machine", since the host's `sftp` and `scp` both ride it. It would also
be the first thing to exercise the channel layer beyond one interactive
session. The gap it runs into is attributes: with no `chmod`-shaped syscall,
`SSH_FXP_SETSTAT` can only honour size and times, and should say so rather than
pretend. Sketched in engram.

`syscallfuzz` shipped and found two kernel panics, both since fixed: an `ioctl`
that wedged its own CPU and a `#GP` on a non-canonical user pointer. The
post-mortems are in [`WORKING-NOTES.md`](WORKING-NOTES.md).

### Blocked, and on what

- `chmod`, `chown` — wait on file permissions (open in engram: `engram-cli todo
  list`). `whoami` and `id` shipped, because they need no permission model:
  `SYS_GETUID`/`SYS_GETGID` already report the ids, and naming them takes only
  the fixed table in `edos_lib::process::id_name`, standing in for a
  `/etc/passwd` that does not exist.
- Hard links — the kernel has no concept of them, which is why `fs::hard_link`
  is one of the three deliberate `unsupported()` stubs in the std fork.
