# Cleanup roadmap

Hygiene, deduplication and interface work. Not features, not bugs, not speed;
the point is that the tree stays cheap to change. Nothing here was found by
running the system: it came from reading it, measuring it, and in two cases
compiling a modified copy of it.

Where a claim has a number behind it, the command that produced the number is
named. Where it does not, the entry says so.

**State as of 2026-08-26.** 25 of the 32 entries are struck; G2 is struck with
a named remainder. What is open, in the order the evidence suggests: C2 (the
parser exists, 125 programs have not adopted it), B2/B3 (the 477 `errno =`
assignments and the dispatch match), H1 and H3 (two long functions), G3 (split
`WORKING-NOTES.md`), and the three gates I2, I5, I6. The numbers each entry
quotes were remeasured on this date.

The conventions these entries are written against, and the sources behind them,
are `doc/rust-style.md`. I5 and I6 are its measurements; the rest of this file
predates it and does not depend on it.

## How to use this file

Each item says **where**, **what is wrong**, **the fix**, and **done when**.
Take them one at a time; almost every one is a self-contained commit. Close the
entry in the same commit that closes the work, as `CLAUDE.md` requires.

Severity is blast radius, not effort:

- **S1** a defect, or a shape that has already produced one
- **S2** a real maintenance cost paid on every change to the area
- **S3** tidiness; worth doing when you are already in the file

Effort is a guess at one sitting (**E1**), a day (**E2**), or a project (**E3**).

---

## A. The syscall boundary is written out twice

This is the highest-risk section. `CLAUDE.md` already states the mechanism:
`WindowListEntry` "used to be written out on both sides, where changing one
alone compiled cleanly and made the compositor read garbage", which is why
`libs/window-abi` exists. `libs/syscall-abi` now does the same for the seven
`#[repr(C)]` types the syscalls exchange. `Errno` is the one name still written
out twice, and its second copy is in the Rust fork rather than in this tree.

### A1. ~~Errno is still declared on both sides of the boundary~~ (blocked on the fork)

`libs/syscall-abi` now holds every `#[repr(C)]` type that crosses a syscall --
`PollState`, `SelectFd`, `DirEntry`, `Stat`, `RawStatFs`, `SockAddrIn` -- and
both the kernel and `edos_lib` depend on it rather than declaring their own.
`Timespec` was unified within userspace separately. `grep 'struct DirEntry'`
over the tree returns one hit.

What is left is `Errno`. The kernel declares it in
`kernel/src/syscalls/mod.rs:1408`; userspace reads it as `edos_rt::sys::Errno`,
which lives in the Rust fork at `~/dev/rust`, not in this tree. Moving it into
`syscall-abi` means making `edos_rt` depend on a path outside its own crate,
publishing it, and moving the fork's pin -- the whole `edos_rt` publish loop in
`CLAUDE.md`.

**Done when** `Errno` has one definition, or this item records the decision not
to make `edos_rt` depend on a crate in this tree and says what keeps the two
lists in step instead.

---

## B. ~~The syscall layer has no error convention~~ (done)

It had one written out 477 times. 124 syscalls, of which exactly 6 returned
`Result<_, Errno>`; the other ~118 set the errno by hand and returned a
sentinel, measured on 2026-08-26 as 477 `errno = Errno::` assignments (97 of
them the `Errno::Clear` reset at function entry) and 253 `!0u64` literals under
`kernel/src/syscalls/`.

This was the same defect `CLAUDE.md` documents on the userspace side ("a return
in `[-4095, -1]` is an error and anything else is a result"), seen from the
kernel: every syscall body re-implemented the protocol, so every syscall body
could get it wrong. All six entries below are closed; B2 and B3 were the two
that carried it, and they landed together because both live in
`syscalls/mod.rs`.

### B2. ~~Syscall bodies should return `Result<u64, Errno>`~~ (done)

Every `sys_*` answers `Result<u64, Errno>`. `SyscallRet::into_rax` in
`syscalls/mod.rs` does once what 477 `errno = Errno::` assignments did by hand,
and `fail_with` is the one place that names the reporting convention: set the
thread's errno, answer with the negated code. `grep -rc 'errno = Errno::'
kernel/src/syscalls/` is 0 and `!0u64` appears nowhere in the directory.

The entry-time `errno = Errno::Clear` went with them, 97 of the 477. Nothing
clears errno on success now, which is what POSIX says of it and what `edos_rt`
already assumed: it reads the field only once a call has reported an error.
That also retired the dispatcher's `if ctx.rax == u64::MAX` substitution and
its "returned -1 with no errno set" log, both of which existed because a body
could fail without saying why. A `Result` cannot.

Shared helpers moved with their files rather than staying half-converted:
`on_cwd_path` and `on_dir_path` (fs.rs, twelve path syscalls), `claim_range`
and `current_user_thread` (memory.rs, shared with shm.rs), `socket_arg`,
`socket_arg_nonblock` and `write_sockaddr_out` (net.rs, eleven socket calls).

Two defects the conversion surfaced, both of which had been reporting a failure
nothing could read: `sys_shm_size` returned -1 with no errno at all, and
`sys_sync` could not fail yet was called from `power::quiesce` for its effect —
now split into `sync_all`, which does the work, and `sys_sync`, which is the
syscall over it.

### B3. ~~`syscall_handler` is an 831-line register-unpacking match~~ (done)

`kernel/src/syscalls/table.rs` now holds one list of 124 entries, each
`number, "name", function, (kind: type, ...)`, and hands itself to a macro
named by its caller. `syscall_rows!` in that file expands it into the
`SyscallInfo` array `/proc/syscalls` publishes; `syscall_arms!` in
`syscalls/mod.rs` expands it into `dispatch`, where arguments come off
`rdi, rsi, rdx, r10, r8, r9` in order through a `FromReg` impl per type rather
than an `as` per call site. `syscall_handler` is 72 lines and holds only the
trace bookkeeping, the errno substitution and the signal boundary.

Neither expansion writes a `sys_*` signature, so every implementation stays
where a reader can see it. Eleven arms that were implemented inside the match
became functions to get there: `sys_exit`, `sys_gettid`, `sys_getuid`,
`sys_getgid`, `sys_sched_yield`, `sys_kill`, `sys_sigaction`,
`sys_sigprocmask`, `sys_setpgid`, `sys_getpgid`, and `sys_ioctl`'s arm.

The merge found no drift: all 124 rows agreed with the arms on arity, and every
arm read its registers in ABI order.

### B4. ~~`fs::Error` names one storage driver~~ (done)

`AhciError` was not one variant of `fs::Error`; it was the block-error type the
whole VFS layer was written in, at 91 sites across nine files under `fs/`. So an
NVMe failure was converted into a SATA controller's vocabulary on its way up
(`From<BlockError> for AhciError` collapsed `Cancelled`, `InvalidArg` and
`NoMemory` into `IoError`) and then flattened again to `Errno::EIO`.

`BlockError` is now that type end to end. It gained `thiserror` messages,
`fs::Error::Block(#[from] BlockError)` replaces the AHCI variant, and the
lossy driver conversion is gone along with `fat32`'s `ahci_to_fs`. A block
failure keeps its cause all the way to userspace, where `From<BlockError> for
Errno` gives each one its own number: `ETIMEDOUT`, `EINTR`, `EINVAL`, `ENODEV`,
`ENOMEM`. `AhciError` no longer leaves `drivers/ahci`.

Two AHCI-specific *calls* survive under `fs/`, both device enumeration rather
than error handling: `scan_device` asks `ahci::is_atapi` before parsing a
partition table (`fs/mod.rs:628`), and `devfs/block.rs` names nodes from
`ahci::api::list_devices` beside the NVMe and ramdisk id ranges. Neither is a
type; both want an `AsyncBlockDevice` method instead, which is its own item.

### B5. ~~`Error::IoError` is the catch-all for 136 sites~~ (done)

`fs::Error::IoError` no longer stands in for a cause the code already knows.
Every filesystem site now names one: a full disk is `NoSpace`, a bad user
pointer is `BadAddress` (EFAULT, not EIO), `rmdir` on a populated directory is
`NotEmpty` (ENOTEMPTY), a filesystem that does not implement an operation is
`Unsupported` (EOPNOTSUPP), a frame allocation that failed is `NoMemory`, and a
`BlockError` reaches the syscall layer as `Error::Block(e)` through the existing
`From` rather than a `map_err(|_| IoError)` closure. A failed mount replies with
the driver's own error instead of EIO. `Error::IoError` across `kernel/src` is
136 → 47, and every one of the 47 belongs to a *different* enum
(`graphics::Error`, `AhciError`, `DevFsError`, `HdaError`); `kernel/src/fs` has
one left, the honest `DevFsError::IoError => fs::Error::IoError` conversion.

`parse_gpt` and `parse_mbr` answer `gpt::PartitionError` rather than `&'static
str`, so a failed partition scan carries the block layer's cause instead of a
string literal, and a table refused for its signature is distinguishable from
one refused for I/O. The duplicated `read_sectors_vec` (and its ignored `_buf`
parameter) is one `gpt::read_sectors`.

`map_err(|_| ...)` across `kernel/src` is 59. What is left discards nothing the
receiving variant does not already name, except in `drivers/usb/xhci/mod.rs`
(9), `fs/memfs/mod.rs` (9) and `drivers/virtio/gpu.rs` (4), which are their own
items if they are ever worth doing.

### B6. ~~`BlockError::from_code` silently invents a variant~~ (done)

The wildcard arm is `panic!` (not `unreachable!`, which is not const-callable).
It is only ever fed a value `BlockIoHandle::complete` wrote with `e as u32` on
this same enum, so an unrecognised code means the atomic was corrupted, and the
`Ok` path stores 0 while every discriminant is >= 1, so 0 cannot reach it either.

---

## C. `edos_lib` leaks the raw syscall ABI to 75 programs

`edos_lib` exists to wrap the syscall surface. Its public return types
(`grep -rh 'pub fn .*->' programs/edos_lib/src/*.rs`):

```
 44  i64        raw negated errno
 14  bool
 11  isize      raw
 10  u64        sentinel
 16  Result<...>
 11  Option<...>
```

So the wrapper mostly does not wrap. `CLAUDE.md` already describes the bug this
produces ("A hang where a child never appears to run is this bug: the spawn
failed, the sentinel check missed it"), and documents the workaround: prefer
`spawn_program_with_fds`, which answers `Option`, over `spawn`, which does not.

That is a note telling callers which of two functions in one module is safe. The
fix is to make both safe.

### C1. ~~Give every `edos_lib` entry point a typed failure~~ (done)

`process.rs` is done: every entry point there answers `Result<T, Errno>`, except
`execve` and `reboot`, which return only on failure and so answer the `Errno`
itself, and `getpid`, which cannot fail. `sys_kill` is gone; it was `kill` under
another name. The `CLAUDE.md` paragraph naming which of the two spawns is safe
is gone with it.

`io.rs`'s path and metadata group is done too: `access`, `faccessat`,
`truncate`, `utimensat`, `futimens`, `set_file_times`, `symlink`, `symlinkat`,
`renameat`, `mkdirat`, `mkfifoat`, `mkfifo` and `unlinkat` answer
`Result<(), Errno>`, and `readlink`, `readlinkat` and `getdents` answer
`Result<usize, Errno>`. The two shapes those collapse to are
`sys::sys_ok` and `sys::sys_count`, which `process.rs` now uses as well.

`time.rs::nanosleep` answers `Result<(), Errno>` too, and it was the only
sentinel left outside `io.rs`: the `-> i64` and `-> u64` signatures counted here
in `mounts.rs`, `net.rs`, `procinfo.rs`, `profile.rs` and `trace.rs` are
genuine values (byte totals, a dropped-record count, an errno a `NetError`
already carries), not failures in disguise, and those modules already answer
`Option` or `bool` where they can fail.

`io.rs`'s descriptor group is done as well: `open` and `openat` answer
`Result<u64, Errno>`, `ioctl` answers the request's own `Result<u64, Errno>`,
`close` and `set_winsize` answer `Result<(), Errno>`, and `sys_read`,
`sys_write`, `pread`, `pwrite`, `readv`, `writev` and `poll` answer
`Result<usize, Errno>`. Two shapes are not covered by the rule and were left
alone in `process.rs`: `close` there answers `i32` and `waitpid` answers `-1`
for a failed wait, neither of which is `i64`, `isize` or a sentinel `u64`.

`mem.rs` is C3's: `mmap` answers `Result<NonNull<u8>, Errno>` and `munmap`,
`mprotect` and `msync` answer `Result<(), Errno>`. `io.rs` held a duplicate
`mmap`/`munmap` pair that nothing called; it is gone, and with it the last
`u64::MAX` in the module.

**Done.** No `pub fn` in `edos_lib` returns a bare `i64`, `isize` or a sentinel
`u64`.

### C2. The argument parser exists; 125 of 134 programs do not use it yet (S2, E2)

`programs/edos_lib/src/args.rs` is the parser: a `Spec` of `Opt`s, short
clusters (`-abc`), attached and separated values (`-n5`, `-n 5`,
`--lines=5`, `--lines 5`), `--` as end-of-options, `-` as the positional that
means stdin, an implicit `--help` that prints and exits 0, and a `usage()`
built from the spec. Two shapes the coreutils needed are in it rather than
around it: `Value::Optional`, a value taken only when attached, which is what
`sed -i[SUFFIX]` means; and `Spec::numeric`, which makes a bare `-<digits>`
a value for a named option, which is what `head -20` means.

The nine text coreutils `texttest` covers — `uniq`, `sort`, `cut`, `tr`, `wc`,
`head`, `tail`, `sed`, `grep` — parse through it, and `texttest` asserts that
each answers `--help` on stdout with `usage: <name>` and exit 0, that
`grep -- -pattern` treats the pattern as a pattern, and that `wc -l -` reads
stdin. That is the part of this entry that is done.

Counts, measured as `grep -rl '"--help"\|edos_lib::args' programs/*/src/*.rs`
against 134 program directories:

- 39 accept `--help` (was 29)
- 14 honour `--` as end of options (was 5)
- 9 accept `-` as stdin

**Remaining.** Adopt `Spec` in the rest, starting with the four hand-rolled
short-flag loops still outside it (`gzip`, `ln`, `tee`, `tar`); the others
follow as each program is next touched.

**Done when** the three counts above are all "every CLI program".

### C3. ~~`edos_lib::mem` returns `u64::MAX as *mut u8`~~ (done)

`mmap` answers `Result<NonNull<u8>, Errno>` and `munmap`/`mprotect`/`msync`
answer `Result<(), Errno>`. `NonNull` is load-bearing rather than decorative:
the syscall's failure and a null address are both plausible-looking `u64`s, and
the type is what stops either reaching a `.read()`.

The call-site count in the original entry was wrong -- 46 sites across the two
programs, not 101 -- because it counted every line mentioning a pointer the
mapping produced rather than every line that had to change.

Two duplicates went with it. `io::mmap`/`io::munmap` were a second, uncalled
wrapper pair whose fifth parameter was named `phys_addr` when the kernel reads
that register as a descriptor unless `MAP_PHYSICAL` is set; they are deleted,
and the physical form is `mem::mmap_physical`, which sets the flag itself so the
overload cannot be got wrong. `edos_render/src/graphics.rs` had its own
`PROT_*`/`MAP_*` constants and a raw `syscall5`; it calls `mem::mmap_physical`
now.

---

## D. Rendering has no surface type, so 55 signatures carry one by hand

`edos_render` had a `Surface<'a>` (`programs/edos_render/src/text.rs:52`) used
by the text blitter and nothing else, while everything that drew a rectangle
threaded `(buffer, buffer_width, buffer_height)` through by hand: 55 signatures
took `buffer: &mut [u32]`, the `Widget` trait among them. That is what put 45
functions on seven or more parameters and what kept
`clippy::too_many_arguments` suppressed globally.

D1 and D2 closed the toolkit half and `edos-web` closed the rest. Four
`buffer: &mut [u32]` signatures are left, three of them in `termbench`, which
measures the raw blit on purpose, and one a closure in the same file.

### D1. ~~One surface type, threaded through `Widget::draw`~~ (done)

`Surface` moved out of `text.rs` into `programs/edos_render/src/surface.rs` and
became the receiver for every drawing operation: `rect`, `rect_outline`,
`focus_ring`, `gradient_v`, `fill`, `hline`, `outline`, `text`, `text_in`,
`text_right`, `icon`, `blit`, and `clip_to`. `Widget::draw` takes
`&mut Surface<'_>`; so do `WidgetContainer::draw_all` and `Terminal`'s
`draw_changed`. The free `widgets::draw_*` functions, `theme::draw_gradient_v`
and `Canvas` are gone -- `Canvas` was the same three fields with the same
methods, so programs that had one now hold a `Surface`.

`rect` intersects the clip, which the free function never saw, so a clipped
widget no longer paints its ground outside the clip.

`edos-web` then adopted it: `view::draw`, `ui::toolbar` and `ui::loading_view`
take `&mut Surface`, and the four hand-rolled rasterisers (`fill`, `blit`,
`fill_rounded`, `stroke_rounded`) became three that draw through it. The `top`
those functions threaded by hand is the surface's clip, so a picture scrolled
under the chrome is cut by the same arithmetic a widget is. `wintest` lost its
private `draw_hline` and holds one surface for the whole frame.

### D2. ~~Rectangle rasterisers~~ (done)

`Screen` has one rasteriser now: `Screen::surface()` hands out a
`Surface` over the back buffer with the screen clip already applied, and
`draw_rect`, `fill`, `set_pixel`, `draw_styled_text`,
`draw_texture_transparent` and `blit_pixels_clipped` are all thin callers of
it. `Surface::blit_region` is the one blitter -- a source rectangle, a
destination point and a size, copied row by row -- and `Surface::blit` is a
call to it with the whole source.

`Framebuffer::draw_rect` is not a rasteriser: it posts a rectangle to the
kernel through `/dev/fb` and never touches a pixel in this process, so it
stays where it is.

The rest of the item dissolved rather than being converted: `Texture`,
`DrawRequest` and `Screen` carried a bitmap-font text engine and eighteen
blit/fill/draw primitives that the whole tree never called. `graphics.rs` went
from 2329 lines to 1272. What is left of `Texture` is the cursor bitmap
edos-wm builds, and what is left of `DrawRequest` is the screen's back buffer.

### D3. ~~`draw_rect` bounds-checks per pixel after clamping~~ (done)

The clamp was wrong and the per-pixel test was hiding it, but not the way the
item guessed. `end_x` was `((x + width as i32) as u32).min(buffer_width)`, so a
rect entirely left of the surface -- negative right edge -- cast to a u32 larger
than any surface, clamped to `buffer_width`, and with `start_x` clamped up to 0
filled the **whole row**. The `idx < buffer.len()` test never caught it because
those indices were all in range.

The far edges are computed in `i64` and clamped in that width now, `end_y` is
additionally clamped against the rows the slice really holds, and each row is
one `fill` on a subslice. The per-pixel test is gone because the clamp is the
bounds check.

### D6. ~~The ANSI palette is 16 literals in the terminal widget~~ (done)

`Theme::ANSI` in `programs/edos_render/src/theme.rs` is a `[Color; 16]` indexed
by the ANSI colour number, and the terminal's SGR handler reads it. No
`0xFF......` literal is left in the widget.

---

## E. Dead code and stale suppressions

Measured, not guessed: every `#[allow(dead_code)]` in `kernel/src` was replaced
with an inert `#[cfg_attr(any(), allow(dead_code))]` and the kernel compiled
under the default, `sched-test`, `trace` and `sched-prof` feature sets. The tree
was restored afterwards.

E1 through E4 are closed. Ten bare `#[allow(dead_code)]` survive in the kernel,
each on a single item and each with a doc comment saying why it stays, plus
three `cfg_attr(not(feature = ...))` naming the feature that makes the item
live. There are no blanket allows over a struct or an `impl` block, no
`allow(unused)` and no `expect(unused)` anywhere in `kernel/src`.

### E5. ~~12 `todo!()` / `unimplemented!()` in the kernel~~ (done)

All twelve were `Handler` methods in `kernel/src/acpi/handler.rs`, and all
twelve are implemented: PCI config access routes to the 0xCF8/0xCFC helpers,
`nanos_since_boot` reads the monotonic clock, `stall` spins and `sleep` parks,
and AML mutexes are a fixed reentrant table. `grep -rn 'todo!(\|unimplemented!('
kernel/src` is empty.

---

## F. Duplicated code

### F2. ~~`read_user_path_with_len` and `read_user_path_at` share 12 lines~~ (done)

The shared span was one operation: null check, length bound,
`try_copy_from_user` into a caller-owned `PathBuf`, UTF-8 validate. It is
`copy_user_path_len` in `kernel/src/syscalls/mod.rs`, beside the NUL-terminated
`copy_user_path` it mirrors; the two resolution policies in
`syscalls/fs.rs` are three lines each now and differ only in how they pick the
base directory.

The five front ends the item asked about are three layers, not one repeated:
`util::uaccess` owns the raw copies; `syscalls::mod` owns the two path front
ends (`copy_user_path`, `copy_user_path_len`), both returning a `&str` borrowed
from a caller stack buffer; `syscalls::fs` owns resolution policy
(`read_user_path`, `read_user_path_with_len`, `read_user_path_at`). `copy_in` /
`copy_out` are a different operation -- counted bytes through the heap, so a
caller can copy before taking a lock -- and `read_user_str` /
`copy_user_c_string` answer with owned `CString` / `Vec<u8>` for values that are
not paths. Nothing left to merge.

### F3. ~~`hbox` and `vbox` are one algorithm written twice~~ (done)

`programs/edos_render/src/widgets/layout/linear.rs` holds the one algorithm.
`Axis` answers the six questions that differed between the two copies -- which
size policy, which alignment, which half of a `SizeHint`, which margins, where a
`Rect` starts and how far it reaches -- so the layout body is written once in
terms of a main and a cross axis. `LinearLayout::horizontal()` and
`LinearLayout::vertical()` are the constructors; `HBoxLayout` and `VBoxLayout`
are gone.

`HBox`'s uniform-column pass was never horizontal in anything but its name and
is now `set_uniform`, available on both axes. `HAlign` and `VAlign` were the
same four variants declared twice, which is what made the shared body impossible
to write; they are one `Align`, and `Alignment` still names the two fields.

### F4. ~~`mbr.rs` and `gpt.rs` share 43 lines~~ (done)

The shared span was two partition listings naming the same two enums. Naming
now lives on the enums: `Display for PartitionType` (through `Formatter::pad`,
so the `{:<20}` column still lines up) and `FilesystemType::name`. The MBR
listing had its own, shorter table of names for the same variants and fell back
to `"Other"`; both listings read the one table now.

### F5. ~~`efs-mkfs` re-implements the kernel's extent logic~~ (done)

`efs_common::build_extent_tree` is the one encoder now: it fills the inode's
`data_area`, and hands each finished leaf node to a caller closure that answers
with the block it wrote it to. That is the only part the two sides disagreed
about — the driver reuses the tree blocks the inode already holds, `efs-mkfs`
allocates fresh ones near the file. `max_extents` moved beside it, so the
depth-1 ceiling is stated once as well.

### F6. ~~Button, checkbox and slider share a 21-line block~~ (done)

`FocusState { focused, enabled }` is the state, and `Widget::focus_state` /
`focus_state_mut` is the only thing a widget implements: `focusable`,
`set_focused`, `enabled` and `set_enabled` are trait defaults written against
it. `Label` implements none of them, since a widget that answers `None` gets
"never focusable, always enabled" for free. `Button` and `TextInput` override a
single method each, for the hover/press state one drops and the cursor blink the
other restarts. `Terminal` moved onto it too.

---

## G. Comments and docs

`CLAUDE.md` is explicit: comments document the code and the spec, never the
process that produced the code. The kernel mostly honours this and reads well.
Two pockets do not.

### G2. ~~Restate-the-signature doc comments in `edos_render`~~ (mostly done)

45 doc comments that said only what the signature said were deleted across ten
widget and layout files, `linear.rs` and `grid.rs` included. The ones that state
a constraint stayed (`set_uniform`, `cursor_byte`, `viewport_top`).

Still open: `kernel/src/util/uaccess.rs` has the same voice ("This function
attempts to copy `size` bytes from user space address `src`"). The house style
is `kernel/src/syscalls/io.rs:71`, where the comment on `STREAM_STACK_BUF`
carries a measurement table and says why the constant is small on purpose.

### G3. `doc/WORKING-NOTES.md` is 10,959 lines and `CLAUDE.md` says read it first (S2, E2)

250 sections, 9 marked FIXED or DONE, covering 19 days. `doc/bugs/` holds 24
post-mortems and has a `README.md` stating the format. The line count only ever
grows while this is open, so remeasure it rather than quoting this one.

A handoff document nobody can read is not a handoff. Split it: the current state
and the open traps stay in `WORKING-NOTES.md` and it stays short; each closed
investigation becomes a file in `doc/bugs/`, which is where the tree already
says post-mortems go.

### G4. ~~Five `TODO` comments that are decisions, not notes~~ (done)

All five resolved into statements of what the code does. `timer.rs` says the
HPET is the only wall clock and the PIT is read once for calibration;
`main.rs` says the mount set is compiled in and there is no fstab; `mbr.rs`
says only the four primary entries are walked; `apic/mod.rs`'s "maybe put
behind a loc" became the per-CPU rule the `&'static mut` does not carry. The
scheduler's 65k-queue musing was deleted: the runqueues are intrusive lists and
no such limit exists.

---

## H. Long functions

35 functions exceed 200 lines, 152 exceed 100. Length is not itself a defect, so
these are listed by whether the function does more than one job, not by size.

### H1. `xhci_driver_main`, 755 lines (S2, E2)

`kernel/src/drivers/usb/xhci/mod.rs:1171`. Controller reset, port enumeration,
descriptor fetch, class dispatch and the event loop in one body, with 77 `unsafe`
blocks in the file. Split along the phases it already names in comments.

### H2. ~~`load_elf`, 498 lines~~ (done)

`load_elf` is fifteen lines: resolve the filesystem, refuse one without a page
cache, `map_image(parse_image(path)?, ...)`. The validation boundary is the
`ElfImage` between them.

`parse_image` reads the ehdr, the phdr table and the shdr table and touches no
process state, so a malformed binary fails before anything is mapped -- where
before, a bad relocation was diagnosed only after the tail pages of every
`PT_LOAD` had already been pre-faulted into the address space. Every
attacker-controlled field is validated in exactly one place: `parse_ehdr` for
the identification bytes, class, machine, object type and header entry sizes,
`validate_load_segment` for the user-half bound that `doc/AUDIT.md` §1.1 records
as once missing, and `parse_relocs` for the symbol index and the 4 GiB target
bound. The result is a `Vec<LoadSegment>` whose addresses are already resolved
against the load base and page aligned.

`map_image` consumes that description without re-reading a header field. The
Elf64 offsets live in a private `elf64` module instead of forty consts inside
the function body, and the magic numbers that were bare (`8` for
`R_X86_64_RELATIVE`, `9` for `SHT_REL`, `14` for `SHT_INIT_ARRAY`) are named.

### H3. `sys_read` / `sys_write`, 316 and 289 lines (S2, E2)

`kernel/src/syscalls/io.rs:622` and `:242`. One function per descriptor kind
behind a match, rather than a match with a 300-line body.

B2 was expected to close this and did not: it took 86 lines out of the pair by
deleting their error bookkeeping, which is real but is not the split. The match
over descriptor kinds is what is long, and each arm is still written inline.

### H4. ~~`edos-wm`'s `main`, 607 lines~~ (done)

The event loop is `session.rs` and the window-under-the-pointer arithmetic is
`interaction.rs`, beside the `compositor.rs` and `input.rs` that were already
there. `main` is the seven lines that open a `Session` and run it.

### H5. ~~Long parameter lists~~ (done)

Measured as the count of `#[allow(clippy::too_many_arguments)]` in the tree,
which is exact and is what the lint fires on now that both blanket allows are
off: `git grep -c too_many_arguments -- '*.rs'`. 14 -> 10.

The four that went were `edos-web`'s `blit`, `fill`, `fill_rounded` and
`stroke_rounded`, which took `buffer: &mut [u32], width, height, top` by hand
because that crate had never adopted `edos_render::Surface`. Porting it closed
this and D1's remainder together: the surface carries the buffer, its
dimensions and the clip, and a rounded box takes a `Rect` rather than four
loose numbers.

The ten that remain each carry a reason at the site. Five judged a parameter
object to be the same list one level out (`net/tcp.rs` `build`,
`thread/thread.rs` `new_user`, `syscalls/mod.rs` `do_spawn`,
`usb/mass_storage.rs` `bot_transfer`, `fs/efs/mod.rs` `rename_inner`), and that
judgement stands.

An earlier revision of this entry quoted "45 -> 29 functions taking seven or
more parameters" without recording the command behind it, and no scan
reproduces those figures. The allow count above replaces them because it can be
re-derived.

---

## I. Gates

The point of a gate is that the class does not come back. Each of these
corresponds to a section above.

### I2. A duplicate-block check in CI (S2, E2)

The measurement that produced section F is a longest-common-block diff over
normalised Rust source; it took seconds over 143k lines. Wire it into CI with a
threshold (say, 40 identical non-trivial lines across two files) and an
allowlist. It found F1, F3, F4 and F5 in one pass, and it would have found F1 the
day it was created.

### I3. ~~Take the global `too_many_arguments` allow off~~ (done)

Both blanket allows are gone. With D1 landed the lint had far less to say than
this item assumed: one site in the kernel and eight in userspace, not 45 -- most
of the count was the `buffer, width, height` triple that `Surface` absorbed.

Fixed rather than suppressed: `NvmeOp::new` took ten arguments and now takes
four, with the data half of a command (`completion`, `buffer`, `direction`,
`len`, `bounce`, `prp_list`) in `nvme::cancel_op::OpPayload` and `submitter`
read from the constructing thread, which is the submitting thread by
construction. `edos-edit`'s `draw_status`, and `edos-files`' `draw_toolbar` and
`draw_list`, took a document's status fields and a list's view state as loose
parameters; those are `view::Status`, `view::Nav` and `view::ListState`.

Suppressed per site, with the reason at each: `Screen::blit_pixels_clipped`
(nine numbers from four rectangles), `edos-wm::composite` (one frame's
independent inputs), and `graphics::render_text_wrapped`, which carries its own
buffer, dimensions and clip. `edos-web`'s rasterisers were suppressed here too
and are not any more; porting that crate to `Surface` removed the parameters
rather than the warning.

### I4. ~~A dead-code sweep that is not a lint suppression~~ (done)

`make dead-code` (`scripts/dead-code-sweep`). It neutralises every `dead_code`
allow in `kernel/src` with `#[cfg_attr(any(), ...)]`, checks the default build
and every feature Cargo.toml declares, prints what the compiler then calls
unused, and restores the tree from a copy taken before the first edit -- on a
signal too, and without running git, so a dirty tree is safe. It reports the ten
survivors E1 annotated and nothing else.

### I5. Document the kernel's unsafe, both halves (S2, E3)

`unsafe` is documented twice, for two different readers, and this entry used to
name only the first. `doc/rust-style.md` states the rule and the sources; the
counts below are the gap, measured 2026-08-26 with the commands recorded there.

**I5a, the blocks and the impls.** 767 `unsafe { ... }` blocks against 46
`// SAFETY:` comments, so about 6%, concentrated in `usb/xhci/mod.rs` (77
`unsafe`), `ahci/port.rs` (39), `thread/scheduler.rs` (33), `virtio/gpu.rs` (33)
and `syscalls/mod.rs` (31). `clippy::undocumented_unsafe_blocks` is the gate and
it covers `unsafe impl` too. ~~A bare `unsafe impl Send for T {}` is a hand-made
claim that no data race can arise, in a preemptive SMP kernel with work-stealing,
and it is the claim most likely to stop being true when the type later grows a
field, so the 38 impls came first.~~ **Done:** all 38 `unsafe impl` in
`kernel/src` (18 `Send`, 14 `Sync`, `GlobalAlloc`, `FrameAllocator`) now carry a
`// SAFETY:` naming what makes the claim true, up from 2 with an argument and 6
more with a `// Safety:` the lint does not recognise. What remains here is the
blocks, per module, `memory/` and `syscalls/` first, since turning the lint on
tree-wide at once produces hundreds of findings and would be abandoned.

**I5b, the contracts.** 61 `unsafe fn` declarations against 32 `# Safety`
sections. No lint finds these and none can: a caller of an undocumented
`unsafe fn` has nothing to uphold and cannot be reviewed. Independent of I5a and
the smaller of the two.

**Done when** every `unsafe impl` in `kernel/src` carries a `// SAFETY:`
comment, the lint is denied in `memory/` and `syscalls/` with no suppressions,
and every `unsafe fn` in those two modules carries a `# Safety` section.

### I6. A `[lints]` table, and what goes in it (S3, E1)

There is no `[lints]` table in any manifest in the tree. Clippy runs at its
default level under `-D warnings`, which is why I5's lint has to be enabled
somewhere before it can be enabled per module.

**Fix.** A `[lints.clippy]` table carrying `undocumented_unsafe_blocks`,
`unnecessary_safety_comment`, `unnecessary_safety_doc` and
`allow_attributes_without_reason`, and nothing else; `doc/rust-style.md` records
what was rejected and why. The two `unnecessary_*` lints are why this is a set:
without them the cheapest way to satisfy I5a is a comment that says nothing.

`allow_attributes_without_reason` is a mechanical conversion, not an audit. The
35 `#[allow(` in `kernel/src` and the 17 in `programs/` already carry their
reason as a comment above the attribute (`allocator.rs:161`, `per_cpu.rs:96`,
`block_page_cache.rs:143`); the lint requires it in the `reason = "..."` field,
where a lint can check it.

Same manifests, same sitting: `programs/edos-taskbar`, `programs/edos-terminal`
and `programs/wintest` are still `edition = "2021"` while the other 129 programs,
the kernel and all eight libs are on 2024. None of the three contains `unsafe`,
so this is drift rather than a hole.

**Done when** the table exists, the tree is clean under it, and
`grep -c 'edition = "2021"' programs/*/Cargo.toml` is zero.

---

## J. The pre-commit self-review pass

Adapted from the DDNet preflight gate. Run against the diff, not against your
memory of writing it.

1. **Justify every hunk.** Name the requirement that forces it. A hunk that
   exists only because an earlier version of the change needed it comes out.
2. **Audit every claim.** Each comment you added is a claim. Name the
   measurement, the spec section, or the code path behind it, or delete it. The
   button and checkbox key bug is what an unaudited comment costs: two widgets
   bound to the wrong keys behind a confident comment naming a scancode table
   this system does not use.
3. **Question every piece of work the change adds.** For each new loop, buffer,
   allocation or periodic call: what breaks without it?
4. **Name what became reachable.** Code that was dead and now runs brings its own
   bugs. Section E is the standing list of what is dead today.
5. **Walk the locks.** For every lock the change takes, name its rank and every
   other lock held at the same time. `doc/invariants/lock-order.md` is
   authoritative and is updated in the same commit.
6. **Walk the drop paths.** Anything reachable from a dying thread must be
   non-blocking; see `doc/invariants/drop-contract.md`.
7. **Ask whether you worked around the design.** The signals: you copied a block
   rather than calling into it; the call site needs a comment explaining why it
   does something unusual; you added state whose owner could track it itself; a
   layer learned a detail it has no business knowing. The test is **if this had
   always existed, would the code look like this?** If not, name the structural
   change that would make it look right, then decide deliberately and write down
   which you chose. `WidgetWrapper` was the worked example: a wrapper existed
   only to attach an id, and the comment describing the hazard that created was
   written instead of removing the wrapper.

## K. Rules worth moving into `CLAUDE.md`

These change how code gets written, so they belong where they are loaded while
writing rather than only at review time.

- Anything crossing the syscall boundary is declared once, in a shared crate.
  `libs/window-abi` is the pattern; there is no second pattern.
- A fallible function returns `Result` or `Option`. Sentinels do not cross a
  module boundary, in the kernel or in `edos_lib`.
- A key is named by a `keycode::` constant. Never a numeric literal, never a
  comment asserting what a number means.
- Something drawn goes through a surface, not through a `(buffer, width, height)`
  triple.
- No plan, phase, session or task vocabulary in a comment. Name the behaviour.
- `unsafe` is documented twice and the two are not interchangeable: `// SAFETY:`
  on the block says why this operation upholds a contract, `# Safety` on the item
  says what the contract is. An `unsafe fn` that does unsafe things needs both.
- `#[expect(..., reason = "...")]` over `#[allow]`, so a suppression cannot
  outlive the code that needed it, and on the item rather than on an `impl` block
  or a module. `allow` stays for the three cases where the lint fires
  inconsistently: conditional compilation
  (`#[cfg_attr(not(feature = "x"), allow(dead_code))]` when a feature is what
  makes the item live), inside a macro, and architecture-specific warnings.
- A function that can panic says so in `# Panics`, and an intentional panic
  carries the values in its message. In this tree that message is what reaches
  `run_log.txt`, so it outranks a comment saying the same thing.

`doc/rust-style.md` carries these with their sources and the numbers behind
them.

---

## L. Checked and found clean

Recorded so nobody spends the time again.

- **Clippy** is at zero across the kernel's default, `sched-test`, `trace` and
  `sched-prof` builds. The kernel is genuinely warning-free.
- **`TODO`/`FIXME`/`HACK`/`XXX`** were 5 across 140k lines of Rust and are now
  0: G4 turned all five into statements of what the code does.
- **Colour literals**: 28 `0xFF......` constants outside `theme.rs`, 16 of them
  the terminal's ANSI palette, now 7 -- D6 moved the palette into
  `Theme::ANSI`. The theme discipline is otherwise held.
- **MSI-X setup** is already shared through `drivers::msi::enable_msix_for_device`;
  the five PCI drivers do not each re-implement it.
- **procfs** is table-driven (`GLOBAL_FILES`, `PROCESS_FILES` as `(name, render)`
  pairs). Adding a file is one row, as `CLAUDE.md` claims.
- **`unwrap()` density** is 84 in the kernel and 106 across 130 userspace
  programs. Low enough that a sweep is not worth a roadmap entry; check them
  where you are already working.
- **`libs/window-abi`** does what it is supposed to do. It is the model for A1,
  not a target.

## M. Not investigated

Named so the gaps in this pass are visible.

- `programs/edos-web` (4,596-line `css.rs`, 1,775-line `view.rs`, 1,709-line
  `doc.rs`) was not read. It is the largest single body of code in the tree and
  the only one with a real host-side test suite, so it deserves its own pass.
- `kernel/src/thread/scheduler.rs` (2,466 lines) was not read for structure.
  `doc/SCHED-ROADMAP.md` covers its performance; nothing covers its shape.
- `scripts/edos-vm` (1,205 lines) and the seven other check scripts were not read.
- Module visibility (`pub` that should be `pub(crate)`) was not measured.
