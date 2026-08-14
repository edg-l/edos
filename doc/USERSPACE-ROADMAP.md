# Userspace Roadmap

117 programs and 3 libraries, all in the `programs/` cargo workspace.

## What exists

| Area | Programs |
|---|---|
| Init | `edos-init` (the only process the kernel starts; supervises the GUI session and `sshd`, from compiled-in defaults plus `/etc/services/*.conf`), `svc` (start, stop and inspect them at runtime) |
| GUI | `edos-wm` (compositor, decorations, desktop menu), `edos-terminal`, `edos-taskbar` (panel + applications menu), `edos-files` (file manager), `edos-procview`, `wintest` |
| Shell | `edos-sh` |
| Editor | `edos-edit` (graphical: file tree, tabs, syntax colouring, find), `edos-vi` (PTY, and the only one that works over ssh) |
| Files | `ls`, `cat`, `cp`, `mv`, `rm`, `ln`, `mkdir`, `rmdir`, `mkfifo`, `touch`, `stat`, `find`, `du`, `diff`, `dd` |
| Text | `grep`, `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `tr`, `tee`, `hexdump`, `xargs`, `less`, `sed` |
| Archives | `tar` (ustar create, list and extract), `gzip` and `gunzip` |
| Checksums | `sha256sum` |
| Inspection | `file` |
| System | `ps`, `pstree`, `pmap`, `top`, `lsof`, `free`, `uname`, `nproc`, `dmesg`, `df`, `mount`, `kill`, `sync`, `env`, `printenv`, `id`, `whoami`, `shutdown`, `strace`, `date`, `watch`, `keymap` |
| Install | `edos-install` (installs the live system to a disk), `efs-mkfs` (in-guest EFS format), `fsck` (in-guest EFS check of an unmounted device) |
| Network | `ping`, `dns`, `http`, `wget`, `dnsprobe`, `tcpecho`, `nc`, `sntp`, `httpd`, `netstat`, `sshd` |
| Packages | `grab` (the package manager, lib + CLI), `edos-grab` (its GUI) |
| Audio | `play` |
| Images | `imgview` (BMP and SVG viewer) |
| Games | `snake` |
| Misc | `echo`, `write`, `seq`, `yes`, `sleep`, `true`, `false`, `basename`, `dirname`, `cal`, `hello` |
| Stress tests | `alloctest`, `forktest`, `mmaptest`, `evicttest`, `lockordertest`, `inflighttest`, `threadtest`, `iotest`, `tcptest`, `exectest`, `killtest`, `vectest`, `sigtest`, `fstest`, `socktest`, `stdtest`, `syscallfuzz`, `orphantest`, `fputest` (SSE state across a context switch), `auxvtest` |
| Benchmarks | `fsbench` (filesystem, see `doc/fsbench.md`), `switchbench` and `pollbench` (scheduler and poll paths, see `doc/SCHED-ROADMAP.md`), `balancebench` (placement across CPUs; wants a multi-CPU boot, where the others want one CPU), `latbench` (how long a woken thread waits for a CPU, against the throughput and switches that wait is traded for), `termbench` (the terminal widget with no window and no compositor) |
| Libraries | `edos_lib` (syscall wrappers), `edos_render` (fonts, text, icons, theme, widgets, windows), `edos_http` (HTTP/1.1 over TLS) |

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

**`strace`** (Phase 4, the kernel-aware one that shipped first). A per-thread
trace mark checked at the syscall dispatch choke point, records into a ring the
tracer drains, and `/proc/syscalls` publishing the kernel's own syscall table so
the formatter carries no second copy of it. Follows children, shows what a
blocked process is blocked in, and captures both what a call was handed and what
it filled in. Written up in [`strace.md`](strace.md).

**`tar`** (Phase 3). ustar (POSIX.1-1988), so the archives it writes are the
ones every other implementation reads: `-c`, `-t`, `-x`, with `-v`, `-C`, and
`-f` where `-` or no flag at all means the standard stream, which makes
`tar -cf - dir | tar -xf - -C dest` a directory copy. Regular files,
directories and symbolic links; a member whose name escapes the extraction
directory is refused. Verified both directions against GNU tar rather than only
against itself. See `doc/WORKING-NOTES.md` for the three header details that
are easy to get subtly wrong.

**`top`** (Phase 3). The thread table on a timer, in raw mode. `/proc/processes`
publishes a monotonic `CPUms` per thread, so a *share* of the CPU exists only
between two samples: every percentage is that counter's growth over the interval
just measured, and the first frame reports zero because it has nothing to
subtract from. Sorts by CPU, memory, total time or pid; `-b`/`-n`/`-d` give the
batch form, and a run whose stdout is not a terminal becomes batch on its own so
`top | head` does not write cursor escapes into a pipe. The parser it shares with
`edos-procview` lives in `edos_lib::procinfo`.

**`snake`** (Phase 3). The first program here whose clock nobody drives: it has
to redraw on a timer *and* answer the keyboard, so each frame is one `poll` on
stdin with whatever is left of the tick as its timeout. Sleeping the tick and
then reading would drop keys; reading without a timeout would stop the clock.
The tick deadline is kept outside the input loop, so a keypress redraws the
frame without moving the snake and without pushing the next move back — holding
a direction down would otherwise stall the game. A board cell is two terminal
columns, because a character cell is about twice as tall as it is wide. `-s`
sets the starting tick and the speed ramp is derived from it; `-w` wraps at the
walls instead of dying on them.

**`tcpecho`** (Phase 4). `bind`, `listen`, `accept`, then echo the accepted
descriptor until the peer closes it, one connection at a time so the kernel's
backlog is what holds the next one. The first program to use the listen side of
the stack at all, and it found two kernel bugs on its first run: `sys_listen`
took the port table under the socket lock, inverting the order `handle_tcp`
uses, and closing an accepted socket removed its *listener's* port-table entry,
so a second connection was answered with RST. Both are written up in
`doc/WORKING-NOTES.md`.

**`nc`**. Both ends of a TCP connection over one relay loop: `nc host port`
connects, `nc -l port` listens and accepts, and either way standard input and
the socket are polled together. End of input half-closes the connection rather
than closing it, which is the first use of `SYS_SHUTDOWN` by anything, so
`echo hi | nc host 7` gets its answer back. It found the poll layer refusing to
report a hang-up nobody had asked for, which is why a pipe feeding it hung
forever; both that and the pipe's own poll state are fixed and written up in
`doc/WORKING-NOTES.md`.

**`httpd`**. Serves a directory tree over HTTP with one thread per accepted
connection: `GET` and `HEAD`, an index file or a generated listing for a
directory, content types by extension, `Content-Length` on everything, and 301
for a directory reached without its trailing slash. A request path is percent
decoded before it is checked, so `..` and `%2e%2e` are both 403. It is the
first program to hold several connections open at once, which is how it found
two more listen-path defects, both written up in `doc/WORKING-NOTES.md`.

**`ln`**. Symbolic links have resolved correctly for a long time and there was
no way to make one outside a program. `ln -s` covers the three POSIX shapes:
one target into the working directory, one target to a named link, and several
targets into a directory. There is no `link(2)` and EFS inodes carry no link
count, so `ln` without `-s` reports that rather than pretending.

Two companions, because a link nobody can see is not much of a feature: `ls`
now suffixes a symbolic link with `@` and a directory with `/`, and `stat`
reports the link itself and where it points.

**`nproc`** (Phase 4). The CPU count, from the `/proc/cpuinfo` the kernel
gained for it: vendor, brand string, family/model/stepping and the feature flags
the kernel itself depends on, one block per online CPU, then the detected and
online totals. Two totals rather than one because they differ when an
application processor fails to start, and the answer anything sizing a thread
pool wants is the online count. That is the default; `--all` asks for detected.

**`imgview`** (Phase 4, SVG added in Phase 5). An image viewer: one window, one
picture, either scaled to the window or shown at one image pixel per screen
pixel and cropped. It decodes with the same `edos_render::image` the compositor
uses for a wallpaper, which is what made it small; the part that is not shared
is the policy a *viewer* needs and a ground does not. A wallpaper covers and
crops, because a desktop must have no bare edges; a viewer fits and letterboxes,
because the whole picture is the point. Those two live side by side over one
bilinear resampler.

It reads **SVG** as well, through `resvg`, and the two kinds of source are not
the same thing in different clothes. A raster is resampled, so the viewer
refuses to magnify one past 100%: enlarging hides what the file actually
contains behind a blur. A vector is re-rendered at whatever size the window is,
so there is no blur to hide behind and filling the window is the right default;
the project mark, 22x18 in `/share/icons/edos.svg`, is sharp at 3905% on a
maximized window. The renderer is behind an `svg` feature on `edos_render`
rather than always on, because it is 1.4 MB of code and every graphical program
in the tree links that crate. `edos-files` opens `.svg` through the viewer for
the same reason it does not draw one in its own details pane.

Two limits worth knowing. Text elements are dropped: rendering them means
shaping them, which pulls `fontdb` and `rustybuzz` and a libc this target has
not got, so `usvg` is built without its `text` feature and skips those nodes
while converting. And an SVG is the first image here with real transparency,
while the shell's buffers hold opaque words, so `Svg::render` takes the
background to composite over rather than leaving uncovered pixels black.

**`watch`**. A command re-run on a timer with its output painted over the
previous frame, and `-d` reverse-videoing the columns that changed since the
last run. It runs the command through `sh -c` unless `-x` is given, so
`watch 'ls /bin | wc -l'` is a pipeline, and it reads the child's pipe dry
before waiting on it, since a command whose output exceeds the pipe buffer
would otherwise deadlock the pair.

**`less`**. A pager: the whole text held in memory with a window moved over it,
line and page motion, sideways motion for output wider than the screen, search
with every match on a line reverse-videoed, and `:n`/`:p` across several file
operands. `dmesg | less` is the case that shapes it — the text arrives on stdin,
so the keyboard has to come from somewhere else, and stderr is the descriptor a
pipeline leaves pointing at the PTY. Not a terminal at either end and it is
`cat`, so a pipeline someone else wrote still works.

The escape-aware column splitting `watch` needed is the same thing a pager needs
to scroll sideways and to highlight a match, so it now lives in
`edos_lib::term` (`Cell`, `cells`, `clip`, `window`, `render`) with both
programs over it.

Anything that clips or diffs another program's output has to parse the escape
sequences in it: `ps` colours its state column, so counting escape bytes as
columns clipped every line nine characters short and inserting a highlight in
the middle of one printed its tail as text. `watch` splits a line into columns
that each carry the escapes preceding them. Making the highlight visible also
needed SGR 7 and 27 in `edos_render::widgets::terminal`, which had no reverse
video at all — `top`'s inverse header and status bar had been rendering as
plain text.

**`sshd`**. An SSH-2 server: `curve25519-sha256`, an `ssh-ed25519` host key,
`aes128-ctr` and `hmac-sha2-256`, password authentication, one session channel
with a pty and a shell. A stock OpenSSH client connects with no options.
Written up in [`sshd.md`](sshd.md).

It is the first program to lean on the crypto crates, and they build for
`x86_64-unknown-edos` unmodified — `sha2`, `hmac`, `aes`, `ctr`,
`x25519-dalek`, `ed25519-dalek`, `subtle`, all with `default-features = false`
so nothing reaches for a `getrandom` backend this target has not got.
Randomness is `SYS_GETRANDOM`. The kernel sets `CR4.OSFXSR` but never
`OSXSAVE`, so feature detection reports no AVX and the crates pick their SSE2
backends: that is what makes this safe today, and why enabling `OSXSAVE`
without moving the context switch to `XSAVE` would silently corrupt crypto
state rather than merely slow it down.

Two defects surfaced while testing it, both older than the program and both
fixed here rather than worked around. `exit N` in `edos-sh` dropped its
argument, because `ExecResult::Exit` carried no code and a `-1` sentinel stood
in for the exit request. And a thread parked in `accept` could not be killed at
all, not even by `SIGKILL`: the kill marks the thread and wakes it, but the
death happens at the syscall return boundary, and a wait that re-parks on a
predicate no peer will ever satisfy never reaches that boundary. See
`WaitQueue::wait_until_killable`.

**`edos-files`** (Phase 5). The file manager: a places rail, a listing, a
details pane and a status strip. It is described where it closes a gap, under
Phase 5 item 4 below.

Candidates beyond these phases, ranked by the kernel path each would exercise,
are in [`PROGRAMS.md`](PROGRAMS.md).

## Phase 3: pure userspace, higher complexity

Complete. Everything listed here shipped; see the Done section above.

## Phase 4: kernel-aware

Complete. Everything listed here shipped; see the Done section above.

## Phase 5: what someone else's first hour hits

The active roadmap. Phases 1 to 4 were ranked by the kernel path each program
would exercise, which is what a system needs while it is being built. This one
is ranked by what a person who is not the author runs into after booting the
ISO, in the order they run into it, because that is now the binding constraint
on whether EDOS is usable rather than demonstrable.

Items 1 to 3 are done; 4 and 5 are open. The one part deliberately left is the
runtime theme, and section 3 says why.

Kernel gaps found on the way get fixed on the way. That is not a change of
policy; it is how every earlier phase went, and the fixes are the reason those
programs paid for themselves. The difference is only which end the work is
picked from.

### 1. Keyboard layout, selectable at runtime. Done

`edos_lib::keymap` was one compile-time Spanish ISO table, so everybody who is
not the author booted into a machine where `/`, `-`, `|`, `@` and the quotes
were in the wrong places, and read that as a broken system rather than as one
layout. It was hit before any other feature and misattributed worse than any
other defect here.

A layout is now a table of physical keys, and three of them ship: `us` (the
default), `uk` and `es`. Keycodes name positions rather than characters, which
is why the same constant is a backslash on US and a c-cedilla on Spanish; the
26 letters are shared and a layout overrides one only to hang an AltGr level on
it, as Spanish does for the euro sign. Ctrl+letter is derived from what the key
produces on the layout in force rather than from its position, so a layout that
moves a letter moves its control code with it.

Resolution order, once per process: `keymap=NAME` on the kernel command line,
then `/etc/keymap`, then the built-in default. The boot parameter outranks the
file deliberately, since a layout can be wrong enough that the file carrying it
cannot be typed; the kernel gained `/proc/cmdline` for userspace to read it.
`keymap` reports what is in force and where it came from, and records a choice.
A program resolves its layout at start, so a change reaches programs started
after it: live switching would mean the layout being kernel state that every
program re-reads.

`scripts/edos-vm` encoded the same assumption from the other side and now
assumes the default, which makes its tables very nearly the identity mapping,
since QMP names keys by US position. `doc/vm-control.md` moved with it.

### 2. A clipboard the whole session shares. Done

Copy and paste existed terminal to terminal only, and only because both ends
read the file `/tmp/clipboard`. Nothing else participated.

The kernel owns the buffers now (`kernel/src/window/clipboard.rs`, syscalls 284
and 285), which is what `GUI_PLAN.md` sketched and nobody had implemented. A
file was the wrong shape for something meant to outlive the program that filled
it: `/tmp` is a mount that need not be there.

There are two buffers, as X established. The clipboard is filled by an explicit
copy; the primary selection is filled merely by finishing a selection and
pasted with the middle button, so selecting somewhere does not destroy what was
deliberately copied. Content is bytes, handed back exactly as it arrived.

Who participates: the terminal (Ctrl+Shift+C and V, middle-click paste, and
publishing every finished drag or double-click word to the primary buffer);
`widgets::text_input`, through three defaulted `Widget` methods that the
container binds to Ctrl+C, X and V, so a future widget gets cut, copy and paste
without knowing the clipboard exists; and `vi`, whose `yy`, `dd`, `p` and `P`
go through the same buffer, which is what lets a line yanked in the editor
paste into another window.

Fixed on the way: the container bound focus cycling to scancode 15, which is
Scroll Lock, not Tab.

### 3. Configuration that survives a reboot. Done, except the theme

Nothing a user changed was theirs the next morning: the wallpaper cycled
unrecorded, and shell history went to `/tmp/.sh_history`, which is memfs, so it
died with the boot.

`edos_lib::config` is the mechanism, and it is deliberately smaller than an ini
parser: one setting per file, one value in it, `#` comments allowed above.
That is what the shell can already write with `echo` and read with `cat` when
the graphical program owning a setting will not start. `/etc/keymap` and
`/etc/wallpaper` are the two settings; history moved to `/root/.sh_history`.

A wallpaper is recorded by name rather than by index, `lit:N` for a generated
ground and the path for an image, so the choice survives a file being added to
or removed from `/share/wallpapers`, and can be set by hand. One that no longer
exists falls back to the first generated ground rather than leaving the desktop
bare.

`edos-install` needed no change: it already copies everything but `dev`, `proc`,
`tmp`, `mnt` and `sys`, so moving these out of `/tmp` is exactly what makes them
reach the installed disk. The live session still forgets, since its root is a
ramdisk, which is the right split.

**Still open: the theme.** `Theme::DEFAULT` is a `const`, read in `const`
contexts by about 120 sites across 16 files, so making it selectable is a
const-to-runtime refactor of every colour in the GUI, and it buys nothing until
a second theme exists, which is 60-odd colours that have to be designed rather
than derived. Two separate pieces of work; neither is persistence.

### 4. A desktop that is not a terminal launcher

The window system is genuinely a desktop: edge and corner resize, maximize and
minimize, Alt+Tab, scrollback, drag selection with double-click word snapping.
What it could start was Terminal, Widget demo, Change background, Shut down, so
everything real still happened through a PTY.

**`edos-files` closes the first half of this.** Browsing, opening, renaming,
deleting and making folders are all reachable without a shell, and a picture
opens in `imgview` from a double-click. Three things in it are worth knowing:

- **The places rail is the kernel's mount table**, read through
  `edos_lib::mounts` rather than a list of favourites, so the rail says what is
  mounted and the meter under it says how full the volume under the current
  directory is. `df` and `mount` were converted onto the same decoder, which is
  where the packed reply buffers of `SYS_LIST_MOUNTS` and `SYS_STATFS` now live
  once instead of three times.
- **The size column carries a bar per file**, measured logarithmically across
  the range of sizes present rather than from zero. Measured from zero it says
  nothing: a directory of binaries between 60K and 160K comes out as 111
  identical bars, which was the first version and was rejected on the
  screenshot.
- **It found two defects in `widgets::text_input`**, both of which had been
  there since it was written and neither of which `wintest` could show. Its key
  constants were PS/2 set-1 scancodes while window events carry
  `pc_keyboard` keycodes, so Enter, the arrows, Home, End and Delete did
  nothing and the keys those numbers actually name (`M`, `Z`, `C` and `-`)
  fired the wrong action instead; and the cursor was placed at
  `cursor_pos * char_width()` in a field drawn in the proportional face, so it
  drifted from the text it was supposed to sit in. A second consumer is what
  surfaced both.

The graphical editor that was left has shipped as `edos-edit`: a file tree,
tabs, syntax colouring from a language table, find, go-to-line, and a change
ribbon marking every line that differs from what is on disk. It is the third
consumer of `widgets::text_input`, through its prompt bar. Writing it found no
new defect in that widget -- the two the file manager surfaced were the ones it
had -- but it did find that `Sans-Regular` carries no `▾` or `▸`, so a glyph
the face lacks draws nothing and the tree's chevrons are icons.

Item 4 is closed. `edos-vi` stays: it is the only editor that works over ssh
and on a serial console.

### 5. A way to get software onto the machine

The ceiling rather than an inconvenience, and the reason it is fifth is that
nothing above it can be worked around, while this one can be lived with until
somebody wants to install something.

No compiler and no package manager is the expected shape for a hobby OS. What
is not expected is that both ways around that are shut: `wget` and `http`
refuse `https://` outright for want of TLS, and nothing exposes inflate, so
`tar` reads uncompressed archives only. Between them that rules out essentially
everything published on the internet.

The machinery for running a foreign binary is already there. `sys_access`
grants `X_OK` unconditionally, since the kernel carries no permission bits, so
a prebuilt binary that arrives, untars and runs needs no new kernel surface at
all. Only the transport is missing, and it splits cleanly:

- `gunzip`, decompress-only inflate (RFC 1951) plus the gzip container
  (RFC 1952). A few hundred lines, no dependency, and it makes every `.tar.gz`
  usable over plain HTTP the day it lands. Do this one first.

  Cheaper than it was: `flate2` on its `miniz_oxide` backend already builds and
  links for this target, since `resvg` pulls both in for the PNG images an SVG
  can embed, and `imgview` ships them today. So "write an inflate" is now a
  choice between a few hundred lines of our own and a dependency that is
  already proven on the target, rather than the only option.
- TLS, which is the larger half. `sshd` established that the RustCrypto stack
  builds unmodified for `x86_64-unknown-edos` with `default-features = false`,
  so this is the second consumer of work already done rather than new ground.
  The constraint recorded there applies unchanged: the kernel sets `CR4.OSFXSR`
  and never `OSXSAVE`, so the crates pick their SSE2 backends, and enabling
  `OSXSAVE` without moving the context switch to `XSAVE` would corrupt crypto
  state silently.

### Not on this list, and why

Multi-user. Every process is uid 0, there is no `setuid`, no permission bits
and no user database, and on a personal machine nobody misses any of it. The
part that is a real property of the shipped ISO is narrower: `sshd` puts that
uid-0 shell on the network behind a password stored in cleartext in
`/etc/sshd.conf`. The fix is public-key authentication, which is already the
first item of the sshd hardening list in [`sshd.md`](sshd.md), not a
users-and-groups project standing in front of it.

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

## The std fork caught up with the syscall table

Every program here is ordinary `std` Rust, so a syscall the fork does not know
about is a syscall no program can use without dropping to `edos_lib`. Nineteen
syscalls had landed without the fork moving; `edos_rt` 0.0.42 and the shims in
`library/std/src/sys/*/edos.rs` on `edos_std_v3` close that gap:

| std API | Was | Now |
|---|---|---|
| `fs::symlink`, `fs::read_link` | `unsupported()` | `SYS_SYMLINK` 88, `SYS_READLINK` 89 |
| `fs::set_times`, `File::set_times` | `unsupported()` | `SYS_UTIMENSAT` 280, the second through `futimens` |
| `FileAttr::modified`/`accessed`/`created` | `unsupported()` | the times `SYS_STAT` already carried |
| `FileType::is_symlink` | always `false` | the kind a directory listing reports |
| `File::read_vectored`/`write_vectored` | `unsupported()`, `is_*_vectored() == false` | `SYS_READV` 19, `SYS_WRITEV` 20 |
| `thread::sleep` | rounded to `SYS_SLEEP_MS`, so sub-ms sleeps were 0 or 1 ms | `SYS_NANOSLEEP` 35 |
| `ReadDir` | one `SYS_LIST_DIR` call sized to the whole directory | `SYS_GETDENTS` 78, a chunk at a time |
| `Path::try_exists` | a full `stat` | `SYS_ACCESS` 21 with `F_OK` |
| `OpenOptions::open` | allocated a `CString` per open | `SYS_OPENAT` 257 takes pointer+length |
| `TcpStream`/`TcpListener`/`UdpSocket::set_nonblocking` | `unsupported()` | `SYS_FCNTL` 72 with `F_SETFL`, honoured by `read`, `recvfrom` and `accept` |

`futimens` is new kernel work rather than a wrapper: `SYS_UTIMENSAT` took a path
and a `File` has only a descriptor, so a null path now means "the file this
descriptor names", which is the POSIX form. `iotest` test 9 covers it.

Three stay unsupported on purpose: `fs::hard_link` (the kernel has no hard
links), `set_times_nofollow` (`utimensat` rejects `AT_SYMLINK_NOFOLLOW`, which
`set_times` cannot honour), and the `File::lock` family (no advisory locking).
`lstat` is `stat`, because nothing below it can decline to follow a link;
`readlink` is what reports on the link itself.

The publish loop this needs is in the README, and a `0.0.z` requirement is
exact, so the fork's pin has to move in the same pass.

## Kernel gaps this roadmap exposes

These are the gaps *userspace programs* expose. For kernel-internal work —
correctness, locking, perf hot spots and missing syscalls — see
[`AUDIT.md`](AUDIT.md), with the prioritised list in `ideas.txt`.

1. **procfs depth.** CPU time, RSS and virtual size are all there now. What a
   `top` still lacks is a *rate*: every counter is cumulative, so the reader has
   to difference two samples itself to get CPU percent.
2. **TCP introspection.** Closed: `/proc/sockets` publishes the connection
   table and the port-table bindings, and `netstat` reads it.
3. **Accept backlog.** A concurrent server will test SYN backlog behaviour.
4. **Large-file I/O.** `sha256sum` and `tar` push files larger than RAM through the
   block and page caches.
5. **Rapid spawn and exit.** `xargs` in loop mode hammers the reaper and zombie
   collection.
6. **CPU count.** Not exposed to userspace at all today.
7. **Named FIFOs.** Closed at eb4dd2f: `SYS_MKFIFOAT` 283 and `/bin/mkfifo`,
   a `FileKind::Fifo` both EFS and memfs store, and an `open` that rendezvouses
   a reader with a writer. `mkfifo f; prog > f & other < f` works. The buffer is
   the `Pipe` that already existed, keyed by inode rather than by path;
   `kernel/src/fs/fifo.rs` has the semantics and `doc/services.md` the use it
   was built for. `AF_UNIX` still does not exist, so a FIFO is the only channel
   between two programs with no common parent.

## Service management, and why it waited on a FIFO

Closed at eb4dd2f; `doc/services.md` is the reference. `edos-init` supervised a
**hardcoded array** with no runtime control of any kind. Both halves shipped:
`/etc/services/*.conf` declares a service in keyword-value lines
(`command`, `args`, `essential`, `shell`, `requires`, `enabled_by`) with the
desktop session still compiled in as the default, and `svc` starts, stops,
restarts and inspects one through a control FIFO at `/var/run/svc.ctl`, with
init publishing `/var/run/svc.status` as the answer.

The shape is daemontools' and runit's, and the alternatives were considered and
rejected rather than overlooked. Shared memory plus a signal works with today's
primitives, but nothing does it that way and a request can be observed
half-written. A TCP listener on loopback needs no new machinery and puts init on
the network stack's critical path, which is the wrong dependency for the first
process. OpenRC's model — no daemon at all, state on disk, and the control
program does the work itself — is a real design rather than a dodge, but it fits
badly when something must own `waitpid` and the restart backoff, which is
exactly what init is for.

No dependency graph and no runlevels. runit does without them, and EDOS has one
real ordering constraint — device nodes — that `requires` already covers.

## Signals: what landed, and what is left

Five kernel items shipped 2026-08-11 (`programs/sigtest` covers all of them);
the shell half has not been written yet.

**Done:**

1. **Process groups.** `Thread::pgid` with `setpgid`/`getpgid`, and
   `Pty.foreground_pgid` in place of a single pid, so Ctrl+C reaches a whole
   pipeline. `kill` takes the POSIX forms: a positive pid is one process, 0 is
   the caller's group, a negative pid is that group. `tcsetpgrp`/`tcgetpgrp`
   hand the terminal to a group, which is what `fg` will need.
2. **Stop and continue.** `SIGSTOP`, `SIGTSTP` and `SIGCONT`, with Ctrl+Z
   wired into the line discipline. A thread suspends at the same boundary
   `killed` uses — a syscall return or a tick out of ring 3 — because that is
   where it provably holds nothing, so a suspended process never sits on a
   filesystem lock. `waitpid` gained `WAIT_UNTRACED` and reports a stopped
   child distinguishably; `/proc` shows `Stopped` and a `PGID` column.
3. **Userspace signal handlers.** `sigaction` takes a real function address and
   a restorer; delivery builds a frame on the user stack at the syscall-return
   boundary and `sigreturn` unwinds it, restoring the full context including
   the interrupted syscall's return value. **A thread that never syscalls never
   runs a handler** — default actions still reach it from the timer tick, so
   Ctrl+C kills a spinning process but cannot be caught by one.
4. **`SIGPIPE`.** A write to a pipe with no reader used to buffer into the
   kernel heap forever; it now raises `SIGPIPE` and returns `EPIPE`.
5. **`SIGCHLD`** is sent to the creator when a child exits.

6. **Shell job control.** `JobStatus` has `Stopped`, a job carries the pid of
   every stage plus the process group they share, and `fg`/`bg` resume one.
   `spawn_pipeline` puts the first stage in a group of its own and the rest
   into it, so one Ctrl+C reaches a whole pipeline; the shell hands the
   terminal over with `tcsetpgrp` and takes it back after every job,
   background ones included. Still open: `fg` resumes a job but leaves it
   marked stopped in `/proc` (see WORKING-NOTES).

**Done:**

- fd-numbered redirection. `Redirects` in `programs/edos-sh/src/command.rs` is
  an ordered list of operations, so `2>file`, `2>&1`, `1>&2` and `&>file` work,
  in a pipeline stage as well as on a plain command. Only descriptors 0, 1 and
  2 can be named, since those are the three `SYS_SPAWN2` gives a child.
- Pathname expansion. `programs/edos-sh/src/glob.rs` matches `*`, `?` and
  `[...]` one path component at a time over `readdir`, for command arguments
  and for a `for` loop's word list. A pattern matching nothing is passed
  through unchanged, a quoted or backslash-escaped word is never a pattern, and
  `*` does not pick up dotfiles. The command word itself is not expanded.

## Done: time is local now

The kernel still keeps time as UTC, which is right, and the session carries a
fixed offset from it in `TZ` — an ISO 8601 offset such as `+02:00`, not a POSIX
zone rule and not an IANA name, since there is no zone database and no DST.
`edos_lib::time::local_time` is the one place that applies it, the panel clock
and `cal` use it, and `programs/date` prints it with `-u` for UTC and a
`+FORMAT` subset. `edos-init` sets `TZ` for the session; `export TZ=…`
overrides it. Making that reach anything meant giving the session an
environment at all: init and the terminal spawned through `SYS_SPAWN`, which
passes no envp, and both use `process::spawn_with_env` over `SYS_SPAWN2` now.
See `doc/WORKING-NOTES.md`.

Still missing: a zone database, DST, and any way to *set* the clock
(`settimeofday`), so the RTC the firmware hands over is the only source.
