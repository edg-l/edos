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

### Would surface a kernel gap, which is the point

| Program | The gap |
|---|---|
| in-EDOS `fsck` | `efs-fsck` is host-side only, so the guest cannot check its own filesystem. This is the difference between an OS that can be repaired and one that has to be re-imaged |

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
