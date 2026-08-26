# Driving the VM Without a Display

`make run` needs a local X or Wayland session: it passes `-display sdl`, and
`virtio-vga,blob=on` needs udmabuf on the host. Over SSH, neither is available.

`scripts/edos-vm` boots the same ISO headless instead, exposing two channels:

| Channel | For | Transport |
|---|---|---|
| VNC or SPICE | a human watching | `127.0.0.1:5901` (display `:1`), or `--display spice` on 5930 |
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

`start` brings the images it is about to boot up to date first. The guest runs
its programs off a disk image, not off `filesystem/bin`, and building a program
does not put it in that image — so `make programs` followed by a bare `start`
used to boot the *previous* binary, which from inside the guest is
indistinguishable from the change having had no effect. Any attached image older
than `filesystem/.manifest` is rebuilt through its own make target before QEMU
starts; `--no-rebuild` boots what is on disk. The trigger is narrow on purpose:
the manifest changes when a program is built and at no other time, so a gate
that crashes a guest and reboots it to test journal replay never has its image
rebuilt underneath it. File mtimes cannot answer this question on their own,
because a running guest writes to its own image and so leaves it *newer* than
the binary missing from it.

`stop` is a power cut: the guest's filesystems are whatever the last writeback
left, and the next boot replays the journal. It refuses while a gate
(`scripts/fs-regression`, `nvme-check`, `guest-check`, ...) is driving the
guest, naming the gate, because there is one QEMU here and cutting power under
a running gate makes it judge an image the guest never finished writing;
`--force` overrides. `start` refuses on the same grounds. Typing `shutdown` in
the guest (`-r` to reboot, `-H` to halt) syncs every filesystem first and then powers the
machine off through ACPI, which is what to use before running `efs-fsck` on the
disk image.

Five make targets drive the guest for you and need no display either:

```bash
make test-headless    # kernel sched-test suite; `make test` needs a desktop
                      # session for its PipeWire audio backend, this does not
make storage-check    # scripts/fs-regression (EFS then FAT32) + scripts/fsbench-run
make guest-check      # the guest's own suites -- iotest, socktest, mmaptest and
                      # fifteen more -- in one boot, judged by their exit codes
make nvme-check       # scripts/nvme-check: an NVMe-root boot, a SATA+NVMe
                      # coexistence boot, the logical_block_size=4096 refusal,
                      # edos-install onto a blank NVMe image, and the watchdog
                      # under nvme_timeout_ms=0
make profile-check    # the sampling profiler end to end: the guest profiles a
                      # known workload and the host has to resolve it to the
                      # function that workload spends its time in
```

All five exit 0 only when the run passed. The sched-test suite reports through
`isa-debug-exit`, so qemu's own status is 1 for a pass and 3 for a failure;
`make test` translates that, and a guest that dies before reporting a verdict is
a failure too.

**`make test` leaves a sched-test ISO behind, and it never reaches a desktop.**
The target rebuilds `edos-x86_64.iso` with `CARGO_FLAGS="--features sched-test"`,
and that kernel runs the suite and stops: the serial log ends at `ALL <N> TESTS
PASSED` and the framebuffer stays black. A `make run-headless` or a
`storage-check` right afterwards boots *that* ISO, since both take the file as
already built, and the symptom is a guest that looks hung rather than one that
is misconfigured. `make all` puts the normal ISO back in about three seconds
(cargo still holds the non-feature artifact, so nothing recompiles). Run it
before driving the guest whenever the last thing you ran was a test target.

**The desktop is 1280x800, and `limine.conf` is what says so.** The bootloader
asks the firmware for that mode and the guest keeps it, so QEMU's `xres`/`yres`
do not decide it and neither does anything on the command line. It is the one
number that sets what a remote display carries per frame, since a window drag
ships its old and new rectangle every time. `scripts/edos-vm` *measures* the
screen (one `screendump`, whose PNG header carries the size) rather than
repeating it, so its pointer commands follow whatever the ISO boots at.

**A rebuilt program does not reach the guest until the disk is rebuilt.** Every
`run` target attaches `sata-disk.img`, and root selection prefers it over the
live-root ramdisk, so the guest runs whatever `/bin` that image holds. `make
all` does not rebuild it: after changing a program, run `make sata-disk.img` or
the guest silently executes the previous binary. A gate whose guest boots with
that disk attached therefore lists `sata-disk.img` as a make prerequisite --
`nvme-check` does -- since the image's mtime moves on every boot and so says
nothing about how old its `/bin` is.

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
(`--vnc-addr`) without `password=on` publishes an unauthenticated console; the
same is true of `--display spice`, which is started with `disable-ticketing`.

### A remote display is capped at 33fps, in QEMU's source

Whatever the guest does, QEMU services its display listeners on a timer:

```c
// include/ui/console.h, unchanged as of qemu 11.1
#define GUI_REFRESH_INTERVAL_DEFAULT    30
```

VNC's own floor is that same constant (`VNC_REFRESH_INTERVAL_BASE`), and
`ui/spice-display.c` sets no interval of its own, so both take the 30ms
default. The compositor presents at 77fps into a 33fps pipe, which means two
of every three frames are coalesced away and the survivors do not arrive
evenly -- and uneven delivery is what "not smooth" looks like even when every
delivered frame is whole.

Measured, dragging a 640x480 window: the guest hands the display **255 MB/s**
of raw pixels, SPICE compresses that to **0.6 MB/s** on the wire, and QEMU
burns 28% of one core. Nothing is starved. The wire is not the problem, the
host CPU is not the problem, and the compositor is not the problem.

Capping the compositor to 30ms to match was tried and **not kept**: it trades
smoothness on a local display, where 74Hz is real, for smoothness on a remote
one, on a hypothesis nobody has confirmed by watching. Nothing here is a guest
defect, so nothing in this repo is the fix; the ceiling is that constant in a
locally built QEMU. Recorded so the measurement does not get made twice.

### SPICE, when VNC is not fast enough

```bash
scripts/edos-vm start --display spice     # then: remote-viewer spice://<host>:5930
```

QEMU's VNC server polls the display on a ~30ms timer, so a viewer sees roughly
33 updates a second however fast the guest paints -- and the guest paints a
window drag at 77fps with 1.5ms to spare, so that timer is the ceiling, not the
compositor. SPICE streams damage as the guest produces it and lets the client
draw the cursor, which is the difference that shows when a whole window moves.

VNC stays the default because it needs nothing on the client beyond a VNC
viewer; SPICE wants `remote-viewer` from the `virt-viewer` package, which ships
an MSI for Windows clients on virt-manager.org. Screenshots,
keystrokes and pointer events all go through QMP either way, so nothing that
drives the guest changes.

---

## Commands

| Command | Notes |
|---|---|
| `start` | `--vnc N`, `--vnc-addr`, `--display vnc\|spice`, `--smp N`, `--mem 2G`, `--accel kvm\|tcg`, `--usb-disk [image]`, `--extra-disk [image]`, `--nvme-disk [image]`, `--nvme-lbs BYTES`, `--nvme-mqes N`, `--no-sata`, `--iso IMAGE`, `--pointer tablet\|mouse` |
| `stop` / `status` | `status` reports pid, run state, VNC address |
| `shot [file]` | writes PNG via QMP `screendump` |
| `type <text>` | `--enter` appends Return, `--delay` paces keystrokes |
| `key <qcode>...` | e.g. `ret`, `ctrl+c`, `alt+f4` |
| `click x y` / `move x y` | `--button left\|middle\|right` |
| `launch [row]` | applications menu by row name, instead of raw pixels |
| `panel` | the panel's controls, by name |
| `press <name>` | click a panel control found by name |
| `windows` | the guest's window registry, by name |
| `focus <name>` | click into a window found by title, `--dx/--dy` for a point in it |
| `log [-n N]` | tails `run_log.txt` |
| `qmp <cmd> [json]` | escape hatch for any QMP command |

`--usb-disk` hangs a `usb-storage` device off the same `qemu-xhci` the keyboard
and mouse use, so the guest reaches it through its own xHCI and USB mass-storage
drivers and registers it as `/dev/sdc`, behind `/dev/sda` (the SATA root) and
`/dev/sdb` (the boot ISO). It takes an optional image path and
defaults to `usb-test.img`, which `make usb-test.img` creates; this is the
headless equivalent of `make run-storage`, which needs a display.

`--extra-disk` attaches a second SATA drive on its own bus (`ide.3`), which the
guest does not boot from, so a test can format, mount and cut power on it
without touching the root. It defaults to `journal-test.img`.

`--nvme-disk` attaches an NVMe controller with one namespace, which the guest
reaches through its own NVMe driver and registers as block device 3000,
`/dev/nvme0n1`. It defaults to `nvme-disk.img`, which `make nvme-disk.img`
creates.

`--no-sata` omits the SATA root disk entirely. It exists for `--nvme-disk`:
`sata-disk.img` and `nvme-disk.img` are built from the same `filesystem/` tree
by the same `efs-mkfs`, so both are bootable EDOS roots, and with both attached
which one becomes root is decided by the `root=UUID=` on the kernel cmdline
rather than by the disk under test. Dropping the SATA disk leaves exactly one
candidate. The same hazard is why `journal-test.img` is built with its own
partition GUID, as the comment on that target in `GNUmakefile` records.

Because the two disks carry different partition GUIDs, the stock ISO's cmdline
names the SATA one and an NVMe-only boot needs an ISO that names the other.
`make edos-nvme.iso` builds it from the same tracked `limine.conf` with the GUID
substituted, and `--iso edos-nvme.iso` boots it. `--nvme-lbs 4096` formats the
namespace with a 4 KiB logical block size, which the driver refuses by name; it
exists for `nvme-check`'s refusal case. `--nvme-mqes N` reports `CAP.MQES = N`
(0's based) on the controller, which is how the driver's clamp of its
128-entry I/O queue request is exercised: `--nvme-mqes 63` boots an NVMe root
on a 64-entry queue pair. See `doc/nvme.md`.

The machine matches `make run`'s devices, including Intel HDA with
`-audiodev none`: the guest driver runs its DMA engine and interrupts with no
host audio sink, which is what exercises `/dev/dsp`. `-audiodev pipewire`, as
`make run` uses, refuses to start without a session bus.

---

## Dragging a window looks bad over SPICE, and the guest is not why

Measured 2026-08-13, dragging a text-filled terminal, same guest and same
build, only the display protocol changed:

| | SPICE | VNC |
| --- | --- | --- |
| pointer positions the guest was told about | 21-40 /s | 5-30 /s |
| to the display | 33-53 MB/s | 118-124 MB/s |
| how it looked | ~5 fps, heavy tearing | fine |

The guest does **more** work over VNC and is told about the pointer **less**
often, and it looks better. So neither the frame rate nor the input rate
predicts what a viewer sees, and a drag that feels like 5 fps is not evidence
of anything wrong inside the machine. Throughout all of the above the
compositor reported 76.9 fps, an interval of 13010us p50 / 13016 p95, and zero
stalls.

What fits the numbers is SPICE's lossy video-stream detector: a large moving
rectangle full of text is exactly what it reclassifies as video, and lossy
video of text smears and lags. `scripts/edos-vm start --display spice
--spice-streaming off` is the knob, and the option is documented as "off keeps
text sharp".

Tearing disappeared entirely at the same time. Two changes can account for
that and this session cannot separate them: `c0143ce` gave the compositor a
back buffer, so it stopped drawing into the live scanout the host reads, and
VNC does not do lossy video of a moving region. The guest-side half was a real
defect either way -- see `doc/design/wm-damage.md` -- but do not assume it was
the whole of it.

**Compare protocols before optimising the guest.** Four separate hypotheses
about the compositor, the client, the flush pattern and the input path all
survived their own measurements and were all wrong; a two-minute VNC
comparison settled it. The instrument that makes this checkable is `moves=N`
in the `wmfps` line -- how many frames in the second saw the pointer somewhere
new. It is the only counter that distinguishes "the machine is slow" from "the
machine was never told to move", and both look identical in every other
number.

## Guest constraints

Two properties of the guest shape everything that drives it. They are not
script bugs, and anything else driving the VM will hit both.

### The keyboard layout is whatever the guest resolved

QEMU delivers scancodes, so a character arrives as whatever the guest's layout
says that physical key means. The layout is a runtime setting
(`programs/edos_lib/src/keymap.rs`): `keymap=NAME` on the kernel command line,
then `/etc/keymap`, then the built-in default, which is US.

A guest on the default layout needs no translation, because QMP names its keys
by US position, and that is what the table in `scripts/edos-vm` now assumes. A
guest carrying `/etc/keymap` with something else in it, an installed machine
somebody configured, decodes the same scancodes differently, and `type` will
produce the wrong characters there: on the Spanish layout the US key for `/`
types `-`, which turns `ls /bin` into `ls -bin`. Boot such a machine with
`keymap=us` on the command line to drive it, since the boot parameter outranks
the file.

### The pointer is absolute, and the guest works that out for itself

The machine uses `usb-tablet`. The guest reads each HID interface's report
descriptor (`kernel/src/drivers/usb/hid/report.rs`) and binds whichever one
describes a pointer, so it learns from the device whether an axis is a position
or a displacement rather than assuming a layout.

`scripts/edos-vm` therefore names a pixel in one event, and asks QEMU
(`query-mice`) rather than assuming: `--pointer mouse` starts a relative
`usb-mouse` instead, which still works and still needs the homing dance below.

**The cursor is not in a screenshot.** Having an absolute pointer let the
compositor put the cursor on the virtio-GPU cursor plane, so a pointer move no
longer damages the framebuffer and a remote viewer draws the cursor itself --
which is what makes it feel immediate over VNC. `screendump` captures the
scanout and not that plane, so a screenshot shows the desktop with no pointer
in it. Do not read that as "the pointer did not move"; ask the guest or click
and observe the result instead.

**A relative mouse is also what makes the pointer's own latency visible.** With
an absolute device QEMU does not grab, so the host keeps drawing its own arrow
at native latency exactly where the guest believes the pointer is; you are
watching the host's cursor and it cannot lag. With a relative device QEMU grabs
and hides it, and the only cursor on screen is the one the guest places. Nothing
about the guest changed between the two -- the absolute case simply covers the
guest's cursor with a zero-latency one. So judge pointer latency with
`--pointer mouse`, and do not read a smooth tablet as evidence that the guest is
keeping up.

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

## Recording the tour video

`scripts/record-tour` drives a scripted session and dumps every frame, for the
video on the project site. It holds one QMP connection and interleaves capture
with input, because QMP serves one client at a time.

```bash
scripts/edos-vm start            # on a freshly built root, so the install is real
scripts/record-tour              # ~50s, writes PPM frames under $TOUR_DIR
ffmpeg -framerate 25 -i ~/.cache/edos-tour/frames/f%05d.ppm \
    -c:v libx264 -pix_fmt yuv420p -crf 25 -preset slow \
    -movflags +faststart edos-tour.mp4
```

**Capture format decides the frame rate, and PNG is the wrong one.** QEMU's
`screendump` encodes PNG inline, which held capture to 11fps; the same loop
writing PPM reaches 250fps, so the cadence is a `sleep` rather than a ceiling.
The cost is ~3 MB a frame, so `TOUR_DIR` must be disk-backed — `/tmp` is tmpfs
here and a 50-second run is 3.6 GB.

The pointer coordinates in the script come from the panel and the applications
menu publishing their own geometry to the kernel log, the same blocks `panel`
and `launch` read. Nothing in it is measured off a screenshot, so a layout
change moves the script's targets with it.

**The video and the site's screenshots go stale silently.** Neither is checked
by anything, and the first tour survived from 0.1.0 to 0.8.0 — through a new
window chrome, a new panel, outline fonts and four new programs — while every
page around it stayed current. Re-record when the desktop's appearance changes,
not when someone notices.

## Addressing the panel by name

The panel's buttons are not windows, so nothing in `/proc/windows` accounts for
them. They publish themselves instead: the panel writes where each of its
controls sits to `/dev/klog` whenever its layout moves, and the applications
menu writes its rows as it opens, both tagged so the block can be picked out of
an interleaved serial log. Nothing in `scripts/edos-vm` knows the panel's
geometry.

```bash
scripts/edos-vm panel             # X, Y, W, H, kind and label, per control
scripts/edos-vm press clock       # click a control found by name
scripts/edos-vm press Terminal    # ...including a task button, by window title
scripts/edos-vm launch            # applications menu, then "Terminal"
scripts/edos-vm launch widgets    # ...or any other row
scripts/edos-vm launch shutdown   # power the machine off through the menu
```

Names match case-insensitively and ignore spaces, preferring an exact match,
then a prefix, then a substring, so the row drawn as "Shut down" answers to
`shutdown`. A name matching several controls is reported rather than guessed
at. The controls are `launcher`, `volume`, `network`, `clock`, and one `task`
per window, named by that window's full title rather than the elided label
actually drawn.

The panel republishes only when its layout changes — a window opening or
closing, or the clock growing a digit — so `panel` and `press` cost nothing but
a read of `run_log.txt`. `launch` does have to wait: it notes where the log ends,
clicks the launcher, and reads the block the menu writes as it opens, so a
previous opening's rows cannot answer for this one.

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
geometry on screen: `scripts/edos-vm press <title>` clicks its task button,
which can. And it reads the log rather than talking to the guest, so a dump
costs a keystroke and about a second.

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

## A program's own output is on the screen, not in `run_log.txt`

`run_log.txt` carries the serial console: the kernel log, panics, and the
`KILL:` lines the page-fault path writes. A program started from the GUI
terminal writes its stdout to that terminal's PTY, and none of it reaches
serial. So `wait_for(mark, "all tests passed", ...)` against the serial log
times out while the line sits on screen, and grepping the tail finds only the
kernel-side traces — for `mmaptest` those are the `PastEof` `KILL:` lines its
own out-of-bounds test provokes, which look like a failure and are the pass.

Read a verdict of that kind with `scripts/edos-vm shot` and look at the image.
Only a program that writes to `/dev/klog`, or the kernel itself, can be waited
on through the log.

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
| Wrong characters typed | layout mismatch; boot with `keymap=us` |
| Pointer stops short of the target | a relative `--pointer mouse` guest, steps sent faster than it polls |
| Clicks do nothing at all | the guest bound no pointer; check `xhci: pointer on interface` in the log |
| Blank or frozen screenshot | guest panicked; check `run_log.txt` |
| No cursor in a screenshot | expected: the cursor is on its own plane, see above |
| `Could not access KVM kernel module` | not in the `kvm` group in this session |
| `Could not set up host forwarding rule 'tcp:127.0.0.1:2323-:23'` | a guest from an earlier run still holds the forward; `stop` finds it even with no pidfile |
| `already running; stop it first` | exactly that, including a guest whose pidfile an aborted `start` removed |
| `refusing to start/stop: pid N (...) holds the guest slot` | a gate is driving the guest; wait for it, or `--force` |
| `no guest on ...; it exited or was stopped` | the QMP socket is gone: the guest exited, or something else stopped it |
| Viewer disconnects | QEMU exited, since QEMU *is* the VNC server |

## usb-tablet, not usb-mouse

Measured 2026-08-15, and it is the whole of a "the window lags behind the
pointer and the cursor wobbles" report from a QEMU-on-Windows guest.

A relative pointing device does not tell the guest where the pointer is, only
how far it moved. So QEMU has to walk the guest's pointer to where the host's
already is, one small delta at a time, and each delta is its own HID report, its
own transfer, its own interrupt. The same scripted drag, counted with
`mouse_reports` in `/proc/gpu_stats`:

| device | endpoint interval | reports for one drag |
|---|---|---|
| `usb-tablet` | `bInterval=4`, 1 ms | 21 |
| `usb-mouse` | `bInterval=7`, 8 ms | 325 |

Fifteen times the reports, drained through a poll that is eight times slower.
The guest keeps a single transfer outstanding on the interrupt endpoint, so 325
reports take about 2.6 seconds to get through, and for the whole of a drag the
pointer is behind a queue it cannot empty. It catches up in bursts, which is the
wobble.

`scripts/edos-vm` defaults to `usb-tablet`, which is the right choice when the
point is to drive the guest rather than to test its input path. `make run`
deliberately keeps `usb-mouse`: a physical mouse is relative, and the relative
path's problems are invisible on an absolute device.

**The guest-side half is fixed too**: an interrupt endpoint now keeps eight
report buffers queued rather than re-arming one at a time. With a single TRB the
controller has nowhere to put a report until the driver has been woken, has read
the last one and has re-armed, so anything the device produces in that window
waits for the next service interval. That is a missed interval per wake, and it
halves an endpoint's usable rate. A physical mouse is a relative device, so this
is the path real hardware takes.

**And the guest can poll its way out of it after all**, which was worth checking
rather than assuming. `bInterval` is the longest a device is willing to wait
between polls, not the shortest it may be asked: an interrupt IN endpoint with
nothing to say answers NAK and costs a transaction. The driver picks the
interval, so HID endpoints are serviced every 1 ms whatever the descriptor
requests. QEMU's `usb-mouse` asks for 8 ms, which capped the drain at 125
reports a second and made those 325 deltas ~2.6 s of backlog; at 1 ms the same
325 clear in under one second, measured. 1 ms is also what a modern pointing
device asks for unprompted, and the floor xHCI allows at full and low speed.

A real mouse does not behave this way. It produces reports at its own rate and
its `bInterval` is chosen to match, so supply and drain are balanced and no
backlog forms. The hundreds-of-deltas burst is an artefact of emulating a
relative pointer whose position has to be walked to wherever the host's already
is. That is why absolute is correct for a VM and why the queue depth is the
right fix for hardware.
