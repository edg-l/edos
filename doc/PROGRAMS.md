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

## The one to write first

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
| `sed`, even just `s///` | Multiplies the value of every text tool already there |
| `sntp` | The wall clock is sampled once at boot and drifts from there. One UDP round trip fixes it, and it is the only thing that would exercise the UDP client path outside `dns` |
| `pstree` | `/proc/processes` already carries a PPID column that nothing renders as a tree. Small, and the supervision structure `edos-init` builds is currently invisible |

### Would surface a kernel gap, which is the point

| Program | The gap |
|---|---|
| `pmap` | `/proc/<tid>/` has `status` and `cmdline` and nothing that shows an *address space*. This kernel's worst bugs have all been in mm — two mappings sharing a page, COW, lazy relocation, file-backed VMAs — and every one of them would have been visible in a `maps` file. The VMA set is already walked for the RSS column, so this is a renderer over data procfs collects |
| syscall fuzzer | Newly possible: `/proc/syscalls` names every call and says which arguments are pointers, lengths and strings, so a fuzzer can generate structurally plausible calls without a hand-written table. Nothing tests the `uaccess` surface as a surface; this drives every dispatch arm with unmapped pointers, absurd lengths and misaligned structs, and `strace` names the call that killed it |
| `lsof` | Needs `/proc/<tid>/fd`. "Which process still has this open" is unanswerable today, and it is the question a file manager or an unmount failure asks first |
| `httpd` | Serves the guest filesystem over TCP. Same untested listen/accept path `nc` reaches, plus concurrent connections, and it doubles as a host-to-guest inspection channel that needs no disk rebuild |
| `nc` | Pairs with the TCP echo server. The listen/accept path has never been run — `tcptest` only proves the client side |
| `netstat` | Needs a read path into `net/tcp.rs`'s `CONNECTIONS`; `/proc/net` covers interface state and not connections |
| `nproc` | The CPU count is not exposed to userspace at all, which is odd for an SMP kernel |
| `dd` | The raw block path has device nodes and `edos-install` uses them; `dd` makes that reachable by hand |
| in-EDOS `fsck` | `efs-fsck` is host-side only, so the guest cannot check its own filesystem. This is the difference between an OS that can be repaired and one that has to be re-imaged |

### Blocked, and on what

- `chmod`, `chown`, `whoami` — wait on users and file permissions (`todo.txt`).
- Hard links — the kernel has no concept of them, which is why `fs::hard_link`
  is one of the three deliberate `unsupported()` stubs in the std fork.
