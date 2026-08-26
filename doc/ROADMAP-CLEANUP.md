# Cleanup roadmap

Hygiene, deduplication and interface work. Not features, not bugs, not speed;
the point is that the tree stays cheap to change. Nothing here was found by
running the system: it came from reading it, measuring it, and in two cases
compiling a modified copy of it.

Where a claim has a number behind it, the command that produced the number is
named. Where it does not, the entry says so.

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

### A1. Errno is still declared on both sides of the boundary (S2, E2)

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

## B. The syscall layer has no error convention

124 syscalls (`grep -c 'const SYS_' kernel/src/syscalls/mod.rs`) and exactly 6
functions in `kernel/src/syscalls/` return `Result<_, Errno>`. The other ~118
set the errno by hand and return a sentinel:

```
477   `errno = Errno::` assignments under kernel/src/syscalls/
 97   of them the `Errno::Clear` reset at function entry
255   `!0u64` literals under kernel/src/syscalls/
```

This is the same defect `CLAUDE.md` documents on the userspace side ("a return
in `[-4095, -1]` is an error and anything else is a result"), seen from the
kernel: every syscall body re-implements the protocol, so every syscall body can
get it wrong, and 380 hand-written assignments is the surface area.

### B2. Syscall bodies should return `Result<u64, Errno>` (S2, E3)

**Fix.** Every `sys_*` returns `Result<u64, Errno>`. One conversion in the
dispatcher does what all 477 assignments do now (`fail_with` in
`syscalls/mod.rs` is the seed): `Ok(v)` clears the errno and
returns `v`, `Err(e)` sets it and returns `!0u64`. The `?` operator then replaces
every `match ... { Err(_) => { info.lock().errno = ...; return !0u64 } }` block
in the tree.

Do it one file at a time, smallest first: `sync.rs` (6 assignments), `shm.rs`
(18), `memory.rs` (30), `window.rs` (49), `fs.rs` (57), `net.rs` (88), `io.rs`
(126), `mod.rs` (86). Each file is its own commit and each is green on its own.

**Done when** `grep -rc 'errno = Errno::' kernel/src/syscalls/` is one site (the
dispatcher) and `!0u64` appears only there.

### B3. `syscall_handler` is an 831-line register-unpacking match (S2, E2)

`kernel/src/syscalls/mod.rs:492`. 124 arms, each one hand-copying `ctx.rdi`,
`ctx.rsi`, `ctx.rdx`, `ctx.r10`, `ctx.r8` into named locals and casting them.
The argument shapes are already written down a second time, correctly, in
`kernel/src/syscalls/table.rs` (124 `sc!` entries), which exists precisely "so
neither can hold a private copy that rots".

**Fix.** One macro taking the number, the target function and the argument list,
emitting both the dispatch arm and the `SyscallInfo` row. A new syscall then
becomes one line that cannot be added to the dispatcher and forgotten in the
table, which is the failure the table's own doc comment warns about.

**Done when** adding a syscall touches one table and `syscall_handler` is under
150 lines.

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

### B5. `Error::IoError` is the catch-all for 136 sites (S2, E2)

`grep -rc 'Error::IoError' kernel/src` gives 136. Combined with 71
`map_err(|_| ...)` in the kernel, most filesystem failures arrive at the syscall
layer with the cause discarded, which is why a failed mount or a failed read is
hard to diagnose without a serial log.

**Fix.** Not a rewrite. Walk the 71 `map_err(|_|` sites and, wherever the source
error is already a `fs::Error` or a `BlockError`, keep it. Add `From` impls
rather than closures: the kernel has 15 error enums and 3 `From` impls between
them.

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

### C1. Give every `edos_lib` entry point a typed failure (S1, E2)

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

`io.rs`'s descriptor group is done as well: `open`, `openat` and `mmap` answer
`Result<u64, Errno>`, `ioctl` answers the request's own `Result<u64, Errno>`,
`close`, `munmap` and `set_winsize` answer `Result<(), Errno>`, and `sys_read`,
`sys_write`, `pread`, `pwrite`, `readv`, `writev` and `poll` answer
`Result<usize, Errno>`. `mmap` no longer collapses a negated errno to `!0`, so
the last `u64::MAX` in the module is gone. Two shapes are not covered by the
rule and were left alone in `process.rs`: `close` there answers `i32` and
`waitpid` answers `-1` for a failed wait, neither of which is `i64`, `isize` or
a sentinel `u64`.

The `-> i64` and `-> u64` signatures counted here in `mounts.rs`, `net.rs`,
`procinfo.rs`, `profile.rs` and `trace.rs` are genuine values (byte totals, a
dropped-record count, an errno a `NetError` already carries), not failures in
disguise, and those modules already answer `Option` or `bool` where they can
fail. `mem.rs` returns `i32`, which C3 covers.

**Done.** No `pub fn` in `edos_lib` returns a bare `i64`, `isize` or a sentinel
`u64`.

### C2. There is no argument parser, so 110 programs each wrote one (S2, E2)

Seven programs hand-roll the same short-flag loop (`gzip`, `ln`, `sort`, `tee`,
`wc`, `uniq`, `tar`); the rest do something ad hoc. The result is not
duplication so much as inconsistency:

- 29 of ~110 CLI programs accept `--help`
- 4 honour `--` as end of options
- 9 accept `-` as stdin

**Fix.** `edos_lib::args`: a small parser over short clusters, long options,
`--`, and `-`, plus a `usage()` that prints and exits 2. Adopt it in the seven
above first; the rest follow as each program is next touched.

**Done when** a new coreutil gets `--help` and `--` without writing either, and
the three counts above are all "every CLI program".

### C3. `edos_lib::mem` returns `u64::MAX as *mut u8` (S3, E1)

`programs/edos_lib/src/mem.rs:48`. A sentinel pointer. `Option<NonNull<u8>>`.

---

## D. Rendering has no surface type, so 55 signatures carry one by hand

`edos_render` has a `Surface<'a>` (`programs/edos_render/src/text.rs:52`), used
by the text blitter and nothing else. Everything that draws a rectangle threads
`(buffer, buffer_width, buffer_height)` through by hand instead. 55 signatures
take `buffer: &mut [u32]`, including the `Widget` trait itself
(`widgets/mod.rs:84`).

This is why the tree has 45 functions with seven or more parameters, and why
`clippy::too_many_arguments` had to be turned off globally.

### D1. ~~One surface type, threaded through `Widget::draw`~~ (mostly done)

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

**Still open:** 12 `buffer: &mut [u32]` parameters remain, all outside the
toolkit: `edos-web/src/view.rs` (6) and `ui.rs` (2) rasterise their own boxes
and rounded boxes, `termbench` owns a bench buffer, and `wintest` has a private
`draw_hline`. Closing those is the rest of D2.

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

### D6. The ANSI palette is 16 literals in the terminal widget (S3, E1)

`programs/edos_render/src/widgets/terminal.rs` carries 16 `0xFF......`
constants. `CLAUDE.md` says colours come from `theme::Theme::DEFAULT`. An ANSI
palette is a legitimately different thing from chrome colours, so give it a name
in `theme.rs` (`Theme::ANSI`) rather than leaving it inline.

---

## E. Dead code and stale suppressions

Measured, not guessed: every `#[allow(dead_code)]` in `kernel/src` was replaced
with an inert `#[cfg_attr(any(), allow(dead_code))]` and the kernel compiled
under the default, `sched-test`, `trace` and `sched-prof` feature sets. The tree
was restored afterwards.

E1 through E4 are closed. Ten bare `#[allow(dead_code)]` survive in the kernel,
each on a single item and each with a doc comment saying why it stays, plus
three `cfg_attr(not(feature = ...))` naming the feature that makes the item
live. There are no blanket allows over a struct or an `impl` block and no
`allow(unused)` anywhere in `kernel/src`.

### E5. 12 `todo!()` / `unimplemented!()` in the kernel (S2, E2)

`grep -rn 'todo!(\|unimplemented!(' kernel/src`. Each one panics the kernel if
reached. Either implement it, or return the errno that says the operation is not
supported. A `todo!()` on a syscall path is a denial of service any user can
reach.

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

### G2. Restate-the-signature doc comments in `edos_render` (S3, E2)

`programs/edos_render/src/widgets/layout/hbox.rs` has, consecutively: "Set the
padding around the layout content.", "Get the current padding.", "Set the
spacing between items.", "Get the current spacing.", "Set the bounds for the
layout.", "Get the current bounds.", "Get the number of items in the layout.",
"Check if the layout is empty.", "Clear all items from the layout."

None of them say anything the signature does not. `util/uaccess.rs` has the same
voice ("This function attempts to copy `size` bytes from user space address
`src`"). Compare `kernel/src/syscalls/io.rs:71`, where the comment on
`STREAM_STACK_BUF` carries a measurement table and explains why the constant is
small on purpose. That is the house style.

**Fix.** Delete the ones that restate. Keep and expand the ones that state a
constraint (`set_uniform_columns` in the same file is a good comment; it explains
what breaks without it).

### G3. `doc/WORKING-NOTES.md` is 10,021 lines and `CLAUDE.md` says read it first (S2, E2)

220 sections, 9 marked FIXED or DONE, covering 17 days. `doc/bugs/` holds 24
post-mortems and has a `README.md` stating the format.

A handoff document nobody can read is not a handoff. Split it: the current state
and the open traps stay in `WORKING-NOTES.md` and it stays short; each closed
investigation becomes a file in `doc/bugs/`, which is where the tree already
says post-mortems go.

### G4. Five `TODO` comments that are decisions, not notes (S3, E1)

```
kernel/src/timer.rs        fall back to the PIT when there is no HPET
kernel/src/apic/mod.rs     "maybe put behind a loc"
kernel/src/main.rs         fstab
kernel/src/thread/scheduler.rs  the 65k queue limit
kernel/src/fs/mbr.rs       extended partitions
```

Low count, which is good. Each is either a real gap (move it to
`engram-cli todo`, where `CLAUDE.md` says pending work lives) or something the
code should simply say it does not support. `apic/mod.rs` is neither; resolve or
delete it.

---

## H. Long functions

35 functions exceed 200 lines, 152 exceed 100. Length is not itself a defect, so
these are listed by whether the function does more than one job, not by size.

### H1. `xhci_driver_main`, 755 lines (S2, E2)

`kernel/src/drivers/usb/xhci/mod.rs:1170`. Controller reset, port enumeration,
descriptor fetch, class dispatch and the event loop in one body, with 77 `unsafe`
blocks in the file. Split along the phases it already names in comments.

### H2. `load_elf`, 498 lines (S1, E2)

`kernel/src/loader/mod.rs:251`. Parses attacker-controlled input, validates it,
allocates VMAs and maps them, in one function. `doc/AUDIT.md` §1.1 records that
the address validation gap in this function was reachable from an ordinary
`mmap`. A parse step that produces a validated description, and a separate map
step that consumes it, makes the validation boundary visible.

### H3. `sys_read` / `sys_write`, 353 and 328 lines (S2, E2)

`kernel/src/syscalls/io.rs:695` and `:267`. One function per descriptor kind
behind a match, rather than a match with a 300-line body. B2 touches these
anyway.

### H4. `edos-wm`'s `main`, 607 lines (S2, E2)

`programs/edos-wm/src/main.rs:503`. The compositor already lives in
`compositor.rs` and input in `input.rs`; the event loop should be a third module,
not the binary's `main`.

### H5. 45 functions take seven or more parameters (S2, E2)

Most are the drawing functions D1 fixes. The rest worth naming:
`kernel/src/net/tcp.rs:118` `build` (10), `kernel/src/thread/thread.rs:1126`
`new_user` (8), `kernel/src/syscalls/mod.rs:2079` `do_spawn` (8),
`tools/efs-fsck/src/repair.rs:441` `repair_link_counts` (8). Each is a
parameter object waiting to happen.

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
independent inputs), and the three hand-rolled rasterisers that carry their own
buffer, dimensions and clip -- `graphics::render_text_wrapped` and `edos-web`'s
`fill`/`fill_rounded`. Those three collapse to one parameter each the day they
take a `Surface`, which is the rest of D2.

### I4. A dead-code sweep that is not a lint suppression (S2, E1)

The E-section measurement is a shell one-liner plus four `cargo check` runs. Make
it a `make dead-code` target: replace the allows with an inert `cfg_attr`, build
each feature set, print the warnings, restore. Run it before a release rather
than every commit.

### I5. Deny `clippy::undocumented_unsafe_blocks` in the kernel (S2, E3)

902 `unsafe` occurrences in `kernel/src`, concentrated in
`usb/xhci/mod.rs` (77), `ahci/port.rs` (39), `thread/scheduler.rs` (33),
`virtio/gpu.rs` (33), `syscalls/mod.rs` (31). Turning the lint on at once would
produce hundreds of findings, so gate it per-module: enable it in `memory/` and
`syscalls/` first, where the safety argument is about user input and is the one
worth writing down.

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
- `#[allow(dead_code)]` goes on the item, never on an `impl` block or a module,
  and carries a reason. Prefer `#[cfg_attr(not(feature = "x"), allow(dead_code))]`
  when a feature is what makes the item live.
- No plan, phase, session or task vocabulary in a comment. Name the behaviour.

---

## L. Checked and found clean

Recorded so nobody spends the time again.

- **Clippy** is at zero across the kernel's default, `sched-test`, `trace` and
  `sched-prof` builds. The kernel is genuinely warning-free.
- **`TODO`/`FIXME`/`HACK`/`XXX`** total 5 across 143k lines of Rust. Unusually
  low; not a problem area.
- **Colour literals**: 28 `0xFF......` constants outside `theme.rs`, 16 of them
  the terminal's ANSI palette (D6). The theme discipline is otherwise held.
- **MSI-X setup** is already shared through `drivers::msi::enable_msix_for_device`;
  the five PCI drivers do not each re-implement it.
- **procfs** is table-driven (`GLOBAL_FILES`, `PROCESS_FILES` as `(name, render)`
  pairs). Adding a file is one row, as `CLAUDE.md` claims.
- **`unwrap()` density** is 84 in the kernel and 107 across 133 userspace
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
