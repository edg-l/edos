# Driving the VM Without a Display

`make run` needs a local X or Wayland session: it passes `-display sdl`, and
`virtio-vga,blob=on` needs udmabuf on the host. Over SSH, neither is available.

`scripts/edos-vm` boots the same ISO headless instead, exposing two channels:

| Channel | For | Transport |
|---|---|---|
| VNC | a human watching | `127.0.0.1:5901` (display `:1`) |
| QMP | scripts and agents | unix socket, line-delimited JSON |

QEMU is itself the VNC server. There is no X server, no compositor, and no
software GL involved; QEMU already rasterizes the guest framebuffer, and VNC and
screenshots both read the buffer it owns.

---

## Quick start

```bash
make run-headless                  # or: scripts/edos-vm start
scripts/edos-vm shot desktop.png   # PNG of the guest framebuffer
scripts/edos-vm click 400 300      # focus a window
scripts/edos-vm type 'ls /bin'     # synthetic keystrokes
scripts/edos-vm key ret            # raw qcodes, '+' combines: ctrl+c
scripts/edos-vm log -n 40          # tail the guest serial log
scripts/edos-vm stop
```

`stop` is a power cut: the guest's filesystems are whatever the last writeback
left, and the next boot replays the journal. Typing `shutdown` in the guest
(`-r` to reboot, `-H` to halt) syncs every filesystem first and then powers the
machine off through ACPI, which is what to use before running `efs-fsck` on the
disk image.

Three make targets drive the guest for you and need no display either:

```bash
make test-headless    # kernel sched-test suite; `make test` needs a desktop
                      # session for its PipeWire audio backend, this does not
make storage-check    # scripts/fs-regression (EFS then FAT32) + scripts/fsbench-run
```

Both exit 0 only when the run passed. The sched-test suite reports through
`isa-debug-exit`, so qemu's own status is 1 for a pass and 3 for a failure;
`make test` translates that, and a guest that dies before reporting a verdict is
a failure too.

**A rebuilt program does not reach the guest until the disk is rebuilt.** Every
`run` target attaches `sata-disk.img`, and root selection prefers it over the
live-root ramdisk, so the guest runs whatever `/bin` that image holds. `make
all` does not rebuild it: after changing a program, run `make sata-disk.img` or
the guest silently executes the previous binary.

`scripts/fs-regression` and `scripts/fsbench-run` both share the boot, focus and
serial-log helpers in `scripts/vmdrive.py`, so a change to how the guest is
driven belongs there rather than in either script.

Watching from another machine, tunnel the VNC port and point any viewer at
`localhost:5901`:

```bash
ssh -L 5901:127.0.0.1:5901 <server>
```

The VNC server is unauthenticated, which is safe only because it binds to
loopback and the SSH tunnel is the authentication. Binding it to a LAN address
without `password=on` publishes an unauthenticated console.

---

## Commands

| Command | Notes |
|---|---|
| `start` | `--vnc N`, `--vnc-addr`, `--smp N`, `--mem 2G`, `--accel kvm\|tcg`, `--width/--height`, `--usb-disk [image]`, `--pointer tablet\|mouse` |
| `stop` / `status` | `status` reports pid, run state, VNC address |
| `shot [file]` | writes PNG via QMP `screendump` |
| `type <text>` | `--enter` appends Return, `--delay` paces keystrokes |
| `key <qcode>...` | e.g. `ret`, `ctrl+c`, `alt+f4` |
| `click x y` / `move x y` | `--button left\|middle\|right` |
| `launch [row]` | applications menu by row name, instead of raw pixels |
| `windows` | the guest's window registry, by name |
| `focus <name>` | click into a window found by title, `--dx/--dy` for a point in it |
| `log [-n N]` | tails `run_log.txt` |
| `qmp <cmd> [json]` | escape hatch for any QMP command |

`--usb-disk` hangs a `usb-storage` device off the same `qemu-xhci` the keyboard
and mouse use, so the guest reaches it through its own xHCI and USB mass-storage
drivers and registers it as `/dev/usb0`. It takes an optional image path and
defaults to `usb-test.img`, which `make usb-test.img` creates; this is the
headless equivalent of `make run-storage`, which needs a display.

The machine matches `make run`'s devices, including Intel HDA with
`-audiodev none`: the guest driver runs its DMA engine and interrupts with no
host audio sink, which is what exercises `/dev/dsp`. `-audiodev pipewire`, as
`make run` uses, refuses to start without a session bus.

---

## Guest constraints

Two properties of the guest shape everything that drives it. They are not
script bugs, and anything else driving the VM will hit both.

### The keyboard layout is Spanish ISO

`programs/edos_lib/src/keymap.rs` hard-codes a Spanish 105-key ISO layout. QEMU
delivers scancodes, so a character arrives as whatever the guest's layout says
that physical key means:

| Character | Key to send | Character | Key to send |
|---|---|---|---|
| `/` | `shift+7` | `-` | `slash` |
| `?` | `shift+minus` | `'` | `minus` |
| `:` | `shift+dot` | `;` | `comma` |
| `\|` | `altgr+1` | `@` | `altgr+2` |
| `\` | `altgr+grave_accent` | `~` | `altgr+4` |
| `[` `]` | `altgr+bracket_left/right` | `{` `}` | `altgr+apostrophe/backslash` |

Sending the US key for `/` types `-`, which turns `ls /bin` into `ls -bin`. The
full table lives in `scripts/edos-vm`; change the guest layout and that table
must change with it.

### The pointer is absolute, and the guest works that out for itself

The machine uses `usb-tablet`. The guest reads each HID interface's report
descriptor (`kernel/src/drivers/usb/hid/report.rs`) and binds whichever one
describes a pointer, so it learns from the device whether an axis is a position
or a displacement rather than assuming a layout.

`scripts/edos-vm` therefore names a pixel in one event, and asks QEMU
(`query-mice`) rather than assuming: `--pointer mouse` starts a relative
`usb-mouse` instead, which still works and still needs the homing dance below.

**With a relative mouse, reaching an exact pixel means homing first**: the
guest clamps the cursor to the screen rectangle and applies no acceleration,
so driving it hard into the top-left corner is a reliable origin to count from,
and a boot-mouse report caps a step at 127px with roughly 12ms between reports.
None of that applies to the tablet.

### Keystrokes go to the focused window

The window manager focuses on click. Click into a window before typing, or the
keystrokes go nowhere. A new terminal also spawns at the *same* geometry as the
existing one, landing exactly on top, so when driving blind never assume which
window is frontmost; `scripts/edos-vm focus <title>` picks the one you mean.

---

## Panel geometry

Prefer the named subcommand over raw pixels, so a layout change is one edit
rather than a hunt through every script:

```bash
scripts/edos-vm launch            # applications menu, then "Terminal"
scripts/edos-vm launch widgets    # ...or any other row
scripts/edos-vm launch shutdown   # power the machine off through the menu
```

> **These coordinates mirror the GUI source and nothing enforces it.**
> `scripts/edos-vm` copies the panel and menu geometry from
> `programs/edos-taskbar/src/{main,panel,menu}.rs`. Move the layout and every
> scripted click silently lands on the wrong row: no compile error, no failing
> test, just wrong behaviour. Update both in the same commit.

The mapping, for a screen `W x H`:

| Target | x | y |
|---|---|---|
| Launcher | 8 to 44 (centre 26) | `H - 20` |
| Menu row *n* (0-based) | 68 | `H - 40 - 185 + 6 + 32n`, plus 13 from row 2 |

**Task buttons are not addressable by index.** They size to their title and the
list is centred, so their position depends on how many windows are open and what
they are called. Address the window instead, which needs no panel geometry at
all — see below.

---

## Addressing windows by name

The kernel publishes its window registry at `/proc/windows`, and the window
manager copies that file into the kernel log on `Ctrl+Alt+W`. The serial console
is the only channel out of a headless guest, so that is how the geometry reaches
the host:

```bash
scripts/edos-vm windows          # ID, PID, rect, state and title, per window
scripts/edos-vm focus Terminal   # click into a window found by title
scripts/edos-vm focus Term --dx 40 --dy 12   # ...at a point inside its client area
```

`focus` matches case-insensitively, preferring an exact title, then a prefix,
then a substring, and takes the topmost of several matches. It clicks a point of
the target that nothing above it covers, so it raises a partly hidden window
rather than focusing whatever is on top of it. A fully covered window is
reported and left alone.

Two things this does not do. It cannot restore a minimized window, which has no
geometry on screen: use the panel's task button or `Alt+Tab`. And it reads the
log rather than talking to the guest, so a dump costs a keystroke and about a
second.

`/proc/windows` reports the *outer* origin and the *client* size, with the
manager's frame as a fourth column, because that is what the kernel routes
pointer events by. The centre of a client area is therefore
`(x + frame.left + w/2, y + frame.top + h/2)`, which `scripts/edos-vm` computes
for you.

---

## Boot readiness

The serial console is written to `run_log.txt`, truncated on every start, so the
file always describes the current run. The kernel spawns only `bin/edos-init`,
which starts the GUI services itself, so wait for the shell rather than for a
kernel spawn line:

```bash
until grep -q '\[Terminal\] Spawned shell' run_log.txt; do sleep 1; done
```

That marker is the last thing to appear, so the prompt is drawable when it does.
With KVM the whole boot takes about 6s; under TCG it is tens of seconds, which is
the practical reason to care about acceleration here.

A boot can also end in a panic, and a readiness loop that only watches for
success waits out its whole timeout when that happens. Watch for both:

```bash
until grep -qE '\[Terminal\] Spawned shell|KERNEL PANIC' run_log.txt; do sleep 1; done
```

---

## Host requirements

- **`/dev/kvm` access.** It is group-owned by `kvm`; membership applies at login,
  so an existing SSH session, a `tmux` server, or a `ttyd` service started before
  the change all keep the old credentials. `scripts/edos-vm` falls back to
  `/usr/bin/sg kvm` when `/dev/kvm` is not writable. The absolute path matters,
  because `sg` is also the name of the `ast-grep` binary.
- **OVMF firmware**, fetched by the makefile into `ovmf/`. Upstream ships every
  architecture in one `edk2-ovmf.tar.xz`, not as loose `.fd` files.
- **The ISO**, from `make all`. It carries its own root, so it boots on its own.
- **`sata-disk.img`**, from `make sata-disk.img`, which every `run` target attaches
  and the kernel prefers over the live root. Without it the guest still boots, but
  nothing written survives a restart, which quietly breaks any script that expects
  state to persist across one.

---

## Troubleshooting

| Symptom | Cause |
|---|---|
| Typing does nothing | no window focused; click into one first |
| Wrong characters typed | layout mismatch, see the Spanish ISO table |
| Pointer stops short of the target | a relative `--pointer mouse` guest, steps sent faster than it polls |
| Clicks do nothing at all | the guest bound no pointer; check `xhci: pointer on interface` in the log |
| Blank or frozen screenshot | guest panicked; check `run_log.txt` |
| `Could not access KVM kernel module` | not in the `kvm` group in this session |
| Viewer disconnects | QEMU exited, since QEMU *is* the VNC server |
