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
| `start` | `--vnc N`, `--smp N`, `--mem 2G`, `--accel kvm\|tcg`, `--width/--height` |
| `stop` / `status` | `status` reports pid, run state, VNC address |
| `shot [file]` | writes PNG via QMP `screendump` |
| `type <text>` | `--enter` appends Return, `--delay` paces keystrokes |
| `key <qcode>...` | e.g. `ret`, `ctrl+c`, `alt+f4` |
| `click x y` / `move x y` | `--button left\|middle\|right` |
| `launch` / `raise N` | taskbar buttons by role, instead of raw pixels |
| `log [-n N]` | tails `run_log.txt` |
| `qmp <cmd> [json]` | escape hatch for any QMP command |

The machine matches `make run`'s devices, including Intel HDA with
`-audiodev none`: the guest driver runs its DMA engine and interrupts with no
host audio sink, which is what exercises `/dev/dsp`. `-audiodev pipewire`, as
`make run` uses, refuses to start without a session bus.

---

## Guest constraints

Three properties of the guest shape everything that drives it. They are not
script bugs, and anything else driving the VM will hit all three.

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

### The mouse is HID boot protocol, so it is relative

`kernel/src/drivers/usb/hid.rs` implements `process_boot_mouse_report` only.
A `usb-tablet` sends absolute reports in a different format and is silently
ignored, so the machine uses `usb-mouse`.

Reaching an exact pixel therefore means homing first: the guest clamps the
cursor to the screen rectangle and applies no acceleration
(`apply_relative_move`), so driving it hard into the top-left corner is a
reliable origin to count from.

A boot-mouse report carries one signed byte per axis, capping a step at 127px.
Reports issued faster than the guest polls its interrupt endpoint are dropped,
so steps need roughly 12ms of spacing. Motion that silently falls short is
almost always this.

### Keystrokes go to the focused window

The window manager focuses on click. Click into a window before typing, or the
keystrokes go nowhere. A new terminal also spawns at the *same* geometry as the
existing one, landing exactly on top, so when driving blind never assume which
window is frontmost; raise the one you want by its taskbar button.

---

## Taskbar geometry

Prefer the named subcommands over raw pixels, so a layout change is one edit
rather than a hunt through every script:

```bash
scripts/edos-vm launch     # click the "+ Term" launcher
scripts/edos-vm raise 0    # raise the first window, 0-based left to right
```

> **These coordinates mirror the GUI source and nothing enforces it.**
> `scripts/edos-vm` copies `TASKBAR_HEIGHT`, `LAUNCHER_X`, `LAUNCHER_WIDTH` and
> `BUTTON_WIDTH` from `programs/edos-taskbar/src/main.rs`. Move the taskbar
> layout and every scripted click silently lands on the wrong button: no
> compile error, no failing test, just wrong behaviour. Update both in the same
> commit.

The mapping, for a screen `W x H`:

| Target | x | y |
|---|---|---|
| `+ Term` launcher | 60 to 124 (centre 92) | `H - 16` |
| Window button *n* (0-based) | `192 + 124n` | `H - 16` |

The real fix is to stop addressing windows by pixel at all: the kernel window
registry already tracks id, pid, rect, z-order and title, so exposing it (via
procfs, or a guest-side control daemon that resolves window *names*) would make
GUI layout changes stop breaking automation. Recorded under "Addressing windows
by name instead of by pixel" in `ideas.txt`.

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
| Pointer stops short of the target | motion steps sent faster than the guest polls |
| Clicks do nothing at all | `usb-tablet` instead of `usb-mouse` |
| Blank or frozen screenshot | guest panicked; check `run_log.txt` |
| `Could not access KVM kernel module` | not in the `kvm` group in this session |
| Viewer disconnects | QEMU exited, since QEMU *is* the VNC server |
