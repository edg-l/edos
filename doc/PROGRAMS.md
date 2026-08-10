# Programs worth writing

Candidates for `programs/`, ranked by what each would *prove* rather than by how
useful it sounds. That filter is how the recent ones earned their place:
`edos-procview` found that procfs reported no per-process memory, and `iotest`
found that absolute symbolic links resolved in the wrong namespace. A program
that exercises a kernel path nothing else touches pays for itself the first time
it runs.

What already exists is inventoried in [`USERSPACE-ROADMAP.md`](USERSPACE-ROADMAP.md);
this file is what is missing and why it would be worth having.

## The two to write first

**`strace`.** The highest-value program on this list. This OS is driven by an
agent through screenshots and a serial log, so the single thing that would most
change debugging is seeing what a process actually asked the kernel for. It
needs a small kernel piece — a per-thread trace flag checked in the SYSCALL
entry path, writing number, arguments and result to a ring — but dispatch in
`kernel/src/syscalls/mod.rs` is already one choke point, so this is a match arm
and a formatter, not a `ptrace` implementation. Every future "the program did
nothing and printed nothing" turns into evidence.

**A file manager.** The obvious hole in the desktop, and it lands on the parts
of the VFS with the least coverage. Drag-to-move is `rename`, which almost
nothing exercises; symbolic links now resolve across mounts and no graphical
program has ever walked `/tmp` and `/var` in one view. It needs no new kernel
surface at all.

## GUI

| Program | What it exercises that nothing does today |
|---|---|
| File manager | `rename`, readdir at scale, symbolic links across mounts, the dentry cache under someone clicking fast |
| System monitor with sparklines | The *rate* gap: every procfs counter is cumulative, so plotting forces differencing. Would pull on `/proc/ahci_stats`, `/proc/block_cache` and per-CPU load |
| Image viewer | `edos_render` already has a BMP decoder and a bilinear scale-to-cover for wallpapers, so this is mostly wiring. Map the file and it exercises file-backed `mmap` |
| Paint | Damage rectangles and shm compositing harder than anything else: small, frequent, scattered dirty rects, the opposite of the window-drag case already measured in `WORKING-NOTES.md` |
| Minesweeper | Right-click. Nothing in the system uses a non-left mouse button, so that path is entirely untested |
| Settings panel | Would force the keymap out of `edos_lib::keymap`'s hardcoded Spanish ISO layout into something selectable at runtime |
| Music player | HDA plus timing under a GUI. `play` proves the codec works; a seek bar proves the DMA ring survives being poked at |
| Disk-usage treemap | Deep recursion through the page cache, and genuinely useful on a 5G development root |
| Calendar popup | Small and concrete: `Action::Clock => {}` in `programs/edos-taskbar/src/main.rs` means the panel clock hovers, is clickable, and does nothing |

## CLI

### Immediate, no kernel work

| Program | Why |
|---|---|
| `ln` | Symbolic links resolve properly now and there is still no way to make one from a shell. The smallest gap-closer on the list |
| `less` | `dmesg \| less` is missing, and `edos-vi` already proves raw mode works |
| `sed`, even just `s///` | Multiplies the value of every text tool already there |
| `watch` | Trivial against procfs, and immediately useful for the poll-and-look debugging this OS gets |
| `tar` | Host-to-guest transfer without rebuilding a disk image |

### Would surface a kernel gap, which is the point

| Program | The gap |
|---|---|
| `nc` | Pairs with the TCP echo server. The listen/accept path has never been run — `tcptest` only proves the client side |
| `netstat` | Needs a read path into `net/tcp.rs`'s `CONNECTIONS`; `/proc/net` covers interface state and not connections |
| `nproc` | The CPU count is not exposed to userspace at all, which is odd for an SMP kernel |
| `dd` | The raw block path has device nodes and `edos-install` uses them; `dd` makes that reachable by hand |
| in-EDOS `fsck` | `efs-fsck` is host-side only, so the guest cannot check its own filesystem. This is the difference between an OS that can be repaired and one that has to be re-imaged |

### Blocked, and on what

- `chmod`, `chown`, `whoami` — wait on users and file permissions (`todo.txt`).
- Hard links — the kernel has no concept of them, which is why `fs::hard_link`
  is one of the three deliberate `unsupported()` stubs in the std fork.
