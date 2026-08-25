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
`libs/window-abi` exists. Only the window ABI got that treatment. Seven other
`#[repr(C)]` types still live in two files.

### A1. Seven ABI structs have two independent definitions (S1, E2)

| type | kernel | userspace |
|---|---|---|
| `DirEntry` | `kernel/src/syscalls/io.rs:134` | `programs/edos_lib/src/io.rs:91` |
| `SelectFd` | `kernel/src/syscalls/io.rs:144` | `programs/edos_lib/src/io.rs` |
| `Timespec` | `kernel/src/syscalls/mod.rs:1776` | `programs/edos_lib/src/time.rs`, `io.rs` |
| `StatFs` | `kernel/src/fs/mod.rs:438` | `programs/edos_lib/src/mounts.rs` |
| `PollState` | `kernel/src/fs/mod.rs:106` | `programs/edos_lib/src/io.rs` |
| `SockAddrIn` | `kernel/src/net/socket.rs` | `programs/edos_lib/src/net.rs` |
| `FstatEntry` / `Stat` | `kernel/src/syscalls/fs.rs:502` | `programs/edos_lib/src/io.rs:344` |

The last row is the worst case: the same layout under two different names, so
grep cannot even pair them.

**They have already drifted.** `kernel/src/syscalls/io.rs:136` documents
`DirEntry::file_type` as `4=device`; `programs/edos_lib/src/io.rs:94` documents
it as `4=fifo`. `FileKind` at `kernel/src/fs/mod.rs:460` makes `Fifo` the fifth
variant, so userspace is right and the kernel comment, on the side that defines
the ABI, is wrong. Nothing broke this time because only the comment moved.

**Fix.** One crate per boundary, the way `libs/window-abi` already does it. A
`libs/syscall-abi` holding these seven types, `#[repr(C)]`, with the meaning of
every enumerated field written once; kernel and `edos_lib` both depend on it and
neither declares the type. Move `Errno` there in the same crate rather than a
second one.

**Done when** no `#[repr(C)]` type crossing a syscall is declared in more than
one crate, and `grep -c 'struct DirEntry' ` over the tree returns 1.

### A2. `Timespec` is declared twice inside userspace alone (S2, E1)

`programs/edos_lib/src/time.rs` and `programs/edos_lib/src/io.rs` both declare
it, in one crate. Fold into A1's crate, or at minimum into one module of
`edos_lib`.

### A3. `strace`'s `Stat` collides with the ABI `Stat` (S3, E1)

`programs/strace/src/main.rs:53` declares `struct Stat { calls, errors, time_ns }`,
a per-syscall counter that has nothing to do with `edos_lib::io::Stat`. Rename to
`CallStats`.

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

### B1. `fail()` is copy-pasted (S2, E1)

`kernel/src/syscalls/trace.rs:375` and `kernel/src/syscalls/profile.rs:14` are
byte-identical:

```rust
fn fail(errno: Errno) -> u64 {
    current_thread_info().lock().errno = errno;
    !0u64
}
```

Two files found the right helper independently and neither shared it. Promote it
to `syscalls/mod.rs` and use it from both. Do this first: it is ten minutes and
it is the seed of B2.

### B2. Syscall bodies should return `Result<u64, Errno>` (S2, E3)

**Fix.** Every `sys_*` returns `Result<u64, Errno>`. One conversion in the
dispatcher does what all 477 assignments do now: `Ok(v)` clears the errno and
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

### B4. `fs::Error` names one storage driver (S2, E1)

`kernel/src/fs/mod.rs:81`:

```rust
#[error(transparent)]
AhciError(#[from] AhciError),
```

A VFS-layer error type with a variant for AHCI specifically. `NvmeError` exists
(`kernel/src/drivers/nvme/mod.rs:58`) and never leaves the driver, so an NVMe
failure flattens to `Error::IoError` while the equivalent SATA failure keeps its
cause. NVMe is now the default root.

**Fix.** `drivers/block_io.rs` already defines the layer-appropriate
`BlockError`. Replace the variant with `Block(BlockError)` and convert in the
drivers.

**Done when** no type under `fs/` names a specific storage driver.

### B5. `Error::IoError` is the catch-all for 144 sites (S2, E2)

`grep -rc 'Error::IoError' kernel/src` gives 144. Combined with 72
`map_err(|_| ...)` in the kernel, most filesystem failures arrive at the syscall
layer with the cause discarded, which is why a failed mount or a failed read is
hard to diagnose without a serial log.

**Fix.** Not a rewrite. Walk the 72 `map_err(|_|` sites and, wherever the source
error is already a `fs::Error` or a `BlockError`, keep it. Add `From` impls
rather than closures: the kernel has 15 error enums and 3 `From` impls between
them.

### B6. `BlockError::from_code` silently invents a variant (S3, E1)

`kernel/src/drivers/block_io.rs:66`: `_ => Self::Io`. An unrecognised code
becomes a plain I/O error with nothing logged. The tree's own rule is
`unreachable!()` in that arm.

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

**Fix.** `i64` and `isize` returns become `Result<usize, Errno>`; `u64` sentinel
returns become `Option<u64>`. `programs/edos_lib/src/process.rs:299` and
`io.rs:587` currently convert a real `is_err` check *back into* `u64::MAX`, so
the information exists and is being thrown away at the boundary; those two are
one-line changes.

Callers then cannot skip the check. `programs/edos-files/src/main.rs:276,282`
and `programs/edos-init/src/main.rs:116` are the current sentinel comparisons and
become `if let Some(pid)`.

**Done when** no `pub fn` in `edos_lib` returns a bare `i64`, `isize` or a
sentinel `u64`, and the `CLAUDE.md` paragraph about which spawn to prefer can be
deleted.

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

### D1. One surface type, threaded through `Widget::draw` (S2, E2)

`programs/edos_render/src/widgets/mod.rs:177`:

```rust
pub fn draw_rect(buffer: &mut [u32], buffer_width: u32, buffer_height: u32,
                 x: i32, y: i32, width: u32, height: u32, color: u32)
```

**Fix.** Extend `text::Surface` to own the clip and the stride, make it the
receiver: `surface.rect(rect, color)`. `Widget::draw(&self, surface: &mut Surface)`.
Every eight-parameter drawing call in `edos_render`, `edos-wm`, `edos-taskbar`,
`edos-files`, `edos-procview` and `edos-web` collapses.

**Done when** `grep -c 'buffer: &mut \[u32\]' ` over `programs/` is 0 and the
global `too_many_arguments` allow can come off (see I3).

### D2. Five independent rectangle rasterisers (S2, E2)

`Framebuffer::draw_rect` (`graphics.rs:128`), `Texture::fill_rect`
(`graphics.rs:1034`), `Screen::fill_rect` (`graphics.rs:1271`),
`widgets::draw_rect` (`widgets/mod.rs:177`), `Canvas::fill`
(`widgets/canvas.rs:31`), plus `edos-web`'s own `fill` (`view.rs:1751`) and
`fill_rounded` (`view.rs:1666`). Same for blitting: eight `blit_*` on `Screen`,
one in `Canvas`, one in `edos-web`, one in `imgview`.

`CLAUDE.md` says text goes through "one blitter". Rectangles and pixel blits
should too. This falls out of D1: once a surface exists, the rasteriser lives on
it and the three types become three ways of *obtaining* a surface.

### D3. `draw_rect` bounds-checks per pixel after clamping (S3, E1)

`widgets/mod.rs:189`: the loop clamps `start_x`/`end_x` against the buffer, then
tests `if idx < buffer.len()` on every pixel anyway. Either the clamp is right
and the test is dead, or the clamp is wrong and the test is hiding it. Decide,
then write the row with `fill` on a slice instead of a per-pixel loop.

### D4. `WidgetWrapper` forwards 15 trait methods by hand (S1, E1)

`programs/edos_render/src/widgets/container.rs:353`. Its own comment names the
hazard:

> a defaulted trait method left off this wrapper takes the default silently, so
> the wrapped widget loses the capability with nothing to show for it at compile
> time.

`CLAUDE.md` repeats it. The comment is the workaround; the wrapper is the
problem. It exists only to attach a `WidgetId` to a `Box<dyn Widget>`.

**Fix.** Store `(WidgetId, Box<dyn Widget>)` in the container's vector and delete
`WidgetWrapper`. 68 lines go, and so does the whole class of bug.

**Done when** no type in `edos_render` implements `Widget` by delegation.

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

### E1. 21 items are dead under every feature combination (S2, E2)

Identical output for all four builds:

```
drivers/ahci/cancel_op.rs:118   field `slot` never read
drivers/block_io.rs:127         variant `Static` never constructed
drivers/mouse/mod.rs:60         fn `new` never used
drivers/usb/xhci/mod.rs:97      fields `scratch_array`, `scratch_pages` never read
drivers/usb/xhci/device.rs:207  field `output_ctx` never read
fs/fat32/mod.rs:59              fn `split_fat_ino` never used
fs/inode.rs:51                  field `kind` never read
fs/journal/mod.rs:524           method `is_safe_to_flush` never used
memory/fault.rs:82              fields `page_idx`, `file_size` never read
memory/fault.rs:84              field `page_idx` never read
syscalls/memory.rs:71           const `MAP_FIXED` never used
syscalls/memory.rs:81           const `MS_INVALIDATE` never used
thread/mod.rs:66                fields `data_base`, `data_size`, `tcb_base`, `tcb_size` never read
thread/broadcast.rs:42          methods `len`, `recv_timeout` never used
thread/cancel.rs:47             method `id` never used
thread/mailbox.rs:172           fn `reply` never used
thread/poll.rs:35               method `is_pending` never used
thread/scheduler.rs:660         method `dump_all_threads` never used
window/input.rs:99              method `len` never used
window/registry.rs:181          method `contains` never used
window/registry.rs:415          method `all_window_ids` never used
```

Three of these are not "delete it" cases and want a judgement instead:

- `syscalls/memory.rs:71` `MAP_FIXED` is a flag userspace can pass and `sys_mmap`
  never honours. That is a missing implementation, not dead code.
- `drivers/usb/xhci/mod.rs:97` `scratch_array`/`scratch_pages` are DMA
  allocations held so the controller's scratchpad is not freed. If so the field
  is doing its job by existing; say that in the allow and keep it.
- `memory/fault.rs:82` `file_size` unread in a file-backed fault descriptor is
  worth a second look before deleting.

**Done when** each of the 21 is deleted, wired up, or carries an allow with a
reason.

### E2. Two blanket allows cover whole `impl` blocks (S2, E1)

`kernel/src/window/registry.rs:142` and `:222` put `#[allow(dead_code)]` on
`impl WindowInfo` and `impl WindowRegistry` entire. That suppresses the lint for
every method in the block, present and future. Two methods inside are in fact
dead (`contains`, `all_window_ids`) and nobody would have been told about the
third.

**Fix.** Delete both, take the per-item allows the compiler then asks for.

### E3. About five allows suppress nothing (S3, E1)

The experiment fired no warning for, among others,
`kernel/src/thread/irqlock.rs:62` (`lock_ranked`),
`kernel/src/thread/thread.rs:309` (`lock_ranks`),
`kernel/src/thread/thread.rs:1441` and `:1450` (`owned_ops_push`,
`owned_ops_remove`, which have 18 call sites between AHCI, NVMe and
`fs/page_fill.rs`). They are leftovers from when the code really was unused.

### E4. Only 2 of 26 kernel allows carry a reason (S2, E1)

`CLAUDE.md`: "where the code must stay, say why in an `#[allow(dead_code)]` with
a comment". `kernel/src/util/ring.rs:36` and
`kernel/src/thread/sched_prof.rs:27` do it right, with `cfg_attr` naming the
feature that makes the item live. That is the model. Everything else is a bare
attribute.

### E5. 12 `todo!()` / `unimplemented!()` in the kernel (S2, E2)

`grep -rn 'todo!(\|unimplemented!(' kernel/src`. Each one panics the kernel if
reached. Either implement it, or return the errno that says the operation is not
supported. A `todo!()` on a syscall path is a denial of service any user can
reach.

---

## F. Duplicated code

### F2. `read_user_path_with_len` and `read_user_path_at` share 12 lines (S2, E1)

`kernel/src/syscalls/fs.rs:37` and `:69`. Both do: null check, length bound,
`try_copy_from_user` into a `PathBuf`, UTF-8 validate, then diverge only in how
the base directory is chosen. Extract the copy, keep the two resolution
policies.

While there: five overlapping user-copy front ends exist for what is one
operation. `copy_in`/`copy_out` (`io.rs:48`, `:125`), `read_user_path`,
`read_user_path_with_len`, `read_user_path_at`, `read_user_str` (`fs.rs`),
`copy_user_path`, `copy_user_c_string` (`mod.rs:3365`, `:3383`), and
`Mapper::copy_to_user` / `write_val_to_user` (`memory/mapper.rs:525`, `:560`).
Decide which layer owns it and delete the rest.

### F3. `hbox` and `vbox` are one algorithm written twice (S2, E1)

`programs/edos_render/src/widgets/layout/hbox.rs` and `vbox.rs` share 78 lines,
including a 56-line identical block. They differ only in which axis is the main
one.

**Fix.** One `LinearLayout` parameterised by `Axis`, with `HBox` and `VBox` as
constructors. Fixing a layout bug currently means remembering to fix it twice.

### F4. `mbr.rs` and `gpt.rs` share 43 lines (S3, E1)

`kernel/src/fs/mbr.rs:472-487` and `gpt.rs:379-394` (16 identical lines), plus
the header block and a 12-line span. Partition-table scanning boilerplate;
extract the shared walk.

### F5. `efs-mkfs` re-implements the kernel's extent logic (S2, E2)

`tools/efs-mkfs/src/populate.rs:588-600` matches `kernel/src/fs/efs/extents.rs:320-332`.
`libs/efs-common` exists exactly to hold what the kernel and the host tools
share. Move it there.

Two implementations of an on-disk format's allocation rule is how an image that
`efs-fsck` calls clean fails to mount.

### F6. Button, checkbox and slider share a 21-line block (S3, E1)

`button.rs:224-244`, `checkbox.rs:218-238`, `slider.rs:300-319`: the
`focusable` / `set_focused` / `enabled` / `set_enabled` quartet with the same
comment. A `FocusState` struct with a `focus_state()` accessor on the trait, or a
small derive-style macro, removes three copies. Do this after D4, which changes
the same file.

---

## G. Comments and docs

`CLAUDE.md` is explicit: comments document the code and the spec, never the
process that produced the code. The kernel mostly honours this and reads well.
Two pockets do not.

### G1. Plan vocabulary is baked into kernel comments (S2, E1)

"Foundation #N", "Phase 3b, Session B", "Task 0.4b", "Phase 0 census", "Phase C1"
appear in at least ten places:

```
kernel/src/thread/thread.rs:1430,1439,1441,1450
kernel/src/thread/cancel.rs:21
kernel/src/thread/waitqueue.rs:18
kernel/src/debug/lock_order.rs:87,387
kernel/src/drivers/ahci/watchdog.rs:5
kernel/src/drivers/ahci/port.rs:723
kernel/src/fs/page_cache.rs:9
kernel/src/fs/journal/mod.rs:527
kernel/src/memory/fault.rs:352
kernel/src/loader/mod.rs:353,626
```

A reader with no access to the plan cannot tell what "the pre-Foundation-#2
behaviour" was, and there is no document in the tree that says. Rewrite each to
name the behaviour: "falls back to parking with no cancellation hookup for that
op".

`kernel/src/thread/waitqueue.rs:18` is the clearest case: "Raised from 32 to 64
(Foundation #5 Task 0.4b)" is a diff note. What the reader needs is why 64 is
enough, which the rest of the comment does say.

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

### I3. Take the global `too_many_arguments` allow off (S2, E1)

`kernel/src/main.rs:10` and `programs/.cargo/config.toml:16` disable it
crate-wide and workspace-wide. That is why 45 seven-parameter functions exist and
why nobody noticed the missing surface type. Six per-site allows already exist in
the kernel (`thread.rs:1125`, `net/tcp.rs:117`, `syscalls/mod.rs:2078`,
`usb/mass_storage.rs:363`, `fs/efs/mod.rs:2947`) and are currently dead, since
the crate-wide allow already covers them.

**Fix.** Do D1 first, then remove the two blanket allows and keep the per-site
ones, each with a reason.

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
   which you chose. `WidgetWrapper` (D4) is the worked example: the comment
   describing the hazard was written instead of removing it.

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
