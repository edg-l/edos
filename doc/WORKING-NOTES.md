# EDOS working notes

What a session acts on: the current state, how to work, and the traps and facts that
decide choices. Open work lives in engram (`engram-cli todo list`), post-mortems in
`doc/bugs/`, and history in git. Anything already in `CLAUDE.md` is not repeated here.

## Current state

### The NVMe hostile-boot wedge is all four CPUs halted, not a live-lock

The one live bug. Engram tracks it ("NVMe hostile boot wedges about 1 in 10"); this is
the reproduction detail.

`edos-nvme-hostile.iso` boots with `nvme_timeout_ms=0`, so the NVMe watchdog fires on
every command and a healthy boot spends minutes resetting the controller. Its serial
output arrives in bursts with quiet stretches; a normal run wrote 317 KB with 1314
watchdog firings. `scripts/wedge-probe` therefore calls a boot wedged after
`SILENCE = 60` s of no serial growth and caps a run at `RUN_CAP = 600` s. Twelve
seconds of quiet is inside the healthy range.

The boot wedges about 1 in 10 on a quiet host (10 runs: 9 pass, 1 wedge). The wedged
state, read over QMP:

```
IDLE_CPU_MASK          0x000000000000000f   (all four CPUs)   stable across 2 s
NVME_INFLIGHT          0x0000000000000001                     stable
WATCHDOG_RESETS        0x0000000000000a24                     stable
query-status           running
CPU#0..3               HLT=1, RIP in Scheduler::take_idle
```

All four vCPUs are halted in the scheduler's idle path, every idle bit is published,
and nothing runs. One NVMe command is in flight and `WATCHDOG_RESETS` is frozen, so
the watchdog thread stopped being scheduled; it did not decide there was nothing to
do. The shape is a lost wakeup. The RIP is build-specific; resolve it with
`scripts/wedge-resolve` or `addr2line` against the ISO's kernel.

Two readings are wrong and must not be reused:

- "A live-lock with `SWITCHES` advancing." `SWITCHES` is `debug::stall::SWITCHES`,
  compiled only under `--features stall-dump`, which the hostile ISO does not carry.
  That reading came from a different build.
- "The NVMe completion path lost a wakeup" is narrower than the evidence. In the
  wedged run's serial, `nvme_watchdog` fires and resets about 250 times a second
  (2596 resets) and then stops, and every other timer-driven kthread (`tcp-retransmit`
  200 ms, `ahci_watchdog` 1 s, `block_writeback` and `journal_committer` about 5 s)
  also fails to run for 60 s. The watchdog sleeps on a 1 ms timer, not a waitqueue.
  Read the bug as "a thread sleeping on a timer stopped waking" before reading the
  NVMe completion dispatcher.

Reproducer, about 10 minutes:

```
make edos-nvme-hostile.iso
WEDGE_OUT=logs/<date>-wedge scripts/wedge-probe 10
```

Next step, not yet taken. The question is which thread is blocked on what, and
registers only ever say "halted". Build the hostile ISO with the stall detector, which
prints every thread with its state and a backtrace after `STALL_MS` (4000 ms) of no
switches, well inside the 60 s silence threshold:

```
make edos-nvme-hostile.iso CARGO_FLAGS="--features stall-dump"
WEDGE_OUT=logs/<date>-wedge scripts/wedge-probe 10
```

A slow but healthy hostile boot may also dump; the dump prints and the boot continues.

Run nothing beside the probe: no build, no other VM, no gate. Host load alone moves
the rate (a 20-run batch read 2 wedges in its first 10 and 5 in the next 6 with builds
running beside the second half). Each run leaves a `.qmp` file in the output directory
with the counters and registers, and a `.log` with that boot's serial.
`WEDGE_ISOS=a.iso,b.iso` alternates two builds within one batch, which is the only
fair A/B.

The two earlier hostile-boot defects are closed and written up in
`doc/bugs/2026-08-26-the-hostile-nvme-boot-is-two-bugs-and-neither-is-the-log.md` and
its two siblings.

## How to work

### The gate set, and which of them build the disk they judge

Warm wall time is on a warm build tree on one host. A cold clone also pays the kernel
build, the whole `programs/` workspace, the ISOs and both disk images.

| gate | what it judges | needs | warm wall |
|---|---|---|---|
| `make -C kernel check` | every feature combination, warning-free | nothing | 16 s |
| `make fmt-check` | rustfmt over kernel, `programs/`, `tools/`, `libs/` | nothing | seconds |
| `make clippy` | kernel and `programs/` at `-D warnings` | `+edos` toolchain | 3-5 s per tree |
| `make host-tests` | host unit tests | nothing | 4 s |
| `make test AUDIODEV=none` | in-kernel `sched-test` suite, 4-CPU KVM | `/dev/kvm` | 10-12 s |
| `make guest-check` | the suites in `scripts/guest-check`'s `SUITES`, one boot, judged by exit code | `/dev/kvm` | 71-84 s |
| `make nvme-check` | five boots (see `CLAUDE.md`) | `/dev/kvm`; builds `edos-nvme.iso`, `edos-nvme-hostile.iso`, `edos-sata.iso`, `fresh-nvme-blank` | 88 s, measured at four boots |
| `make recovery-check` | pause checkpointing, cut power, remount, assert replay | fault-inject ISO, fresh `journal-test.img` | 93 s |
| `make orphan-check` | unlinked-but-open files across a power cut | fresh `journal-test.img`, `efs-fsck` | 82 s |
| `make storage-check` | `fs-regression` over EFS and FAT32, then `fsbench-run` | `/dev/kvm` | 4 min 27 s |
| `make ssh-check` | host OpenSSH client against guest `sshd` | `/dev/kvm`, host `ssh` | 44 s |
| `make profile-check` | sampling profiler end to end | `/dev/kvm` | not measured |
| `make stall-check` | stall detector fires on `edos-stall.iso` | `/dev/kvm` | not measured |

Only `check`, `fmt`, `clippy`, `make test` and `guest-check` run in CI; `doc/ci.md`
§ "What is not covered" has the rest.

Run `make test` before `make guest-check`. `make test` builds `$(IMAGE_NAME).iso` with
`--features sched-test`, and `guest-check` depends on the same ISO path with default
features, so whichever runs second relinks the kernel and ISO; the `guest-check` time
above includes that. Every guest gate also depends on both disk images, so a touched
`filesystem/` adds an `efs-mkfs` of each.

The guest gates hold the single QEMU slot for their whole run (`edos_vm.claim_slot`,
through `scripts/vmdrive.py`). `fs-regression` and `nvme-check` reboot between phases,
so a guest that looks abandoned is usually a live gate's. `pgrep -af
'fs-regression|fsbench-run|guest-check|nvme-check|ssh-check|orphan-check|recovery-check|profile-check'`
names the gate, and `ls -l /proc/<make-pid>/fd/1` names its log.

Gate scripts use `#!/usr/bin/env -S python3 -u`, so progress reaches a redirected log
as it happens. `guest-check`, `nvme-check`, `orphan-check`, `recovery-check` and
`profile-check` take no arguments and print no usage: `--help` boots a guest and runs
the whole gate. `fs-regression` (`--fat32`, `--keep`) and `fsbench-run` (`path`,
`--mode`, `--quick`) take options.

Do not edit `scripts/edos-vm` while a gate runs. `make storage-check` shells out to it
many times, and an edit landing between two calls runs a half-edited script. It fails
as a `CalledProcessError` on `edos-vm start`, which looks like a guest problem.

### A gate that runs nothing still exits 0

`make -C kernel check --features sched-test` does not pass the flag to cargo. make
parses `--features` as its own unknown option, prints its usage, and exits 0, so every
`&&` after it runs with nothing verified. Plain `make -C kernel check` already runs
`cargo check --features <f>` for every feature through `check-features`. A one-off
feature build goes through `CARGO_FLAGS="--features x"` on the ISO target.

A gate chain is evidence only if each command's own output says it ran. Grep the
saved log for the line that proves the work (`Checking edos-kernel`, `ALL <N> TESTS
PASSED`) rather than reading the chain's exit status. An exit code shared with the
harness's own failures (QEMU's startup failure and an `isa-debug-exit` pass are both
1) needs a positive signal from inside the guest.

A new gate, or a new case in one, is trusted only after it has been watched going red:
break the code on purpose (for `wc`, a `+ 1` on the line count), confirm the gate fails
with the expected `FAIL` line in `run_log.txt`, revert.

### `make test` and `scripts/edos-vm` cannot run at the same time

Both attach `sata-disk.img` and `nvme-disk.img` and both point the serial chardev at
`run_log.txt`. Two QEMUs on one image is a corruption hazard, and the second serial
open truncates the first one's log. Run them in sequence. If they ever overlapped,
rebuild the images and discard any measurement taken during the overlap. After a test
target, the ISO on disk is the sched-test one (`doc/vm-control.md`); `make all` before
`edos-vm start`.

### Driving the guest: traps not in `doc/vm-control.md`

- A cursor or damage change tested only on the default boot has not been tested on
  the software-cursor path; boot `edos-vm start --vga std` for that (VBE has no
  cursor plane).
- To resize a window with `edos-vm drag`, aim at its right border: a window listed
  at `X W` by `edos-vm windows` has it at `X + W`, about 2 px wide.
- Per-operation kernel logging (mmap, munmap, spawn, ELF load, thread exit) goes
  through `log_debug!` (`kernel/src/logs.rs`) and is silent unless the command line
  carries `loglevel=debug`. A grep of `run_log.txt` for per-thread exits returns
  nothing by default.
- A program's output reaches `run_log.txt` only through `/dev/klog`: `prog > /dev/klog
  2>&1`. Test programs report failures through `eprintln!`, so `> /dev/klog` alone
  shows only the exit code. stderr arrives unbuffered, one write per fragment,
  interleaved with other logging; reassemble the line, or print failure lines with
  `println!`.
- To debug input, log each key event with `edos_lib::io::klog_dump` and read
  `run_log.txt` rather than inferring from screenshots. A repeated action that sets
  the same status text as before proves nothing by the status not changing.
- `edos-vm start` truncates `run_log.txt`, and `run_log.txt` holds only the latest
  boot. In a loop of boots, or across an install-then-reboot, copy it aside per boot.
- A hung guest that still has a compositor: open a second terminal from the taskbar
  launcher and run `cat /proc/<file> > /dev/klog`. `/proc/<tid>/status` (`Parked`,
  `Sleep Deadline: 0`, unchanging `CPU Time`) separates an indefinite park from a
  spin or a timed sleep; `/proc/nvme_stats`, `/proc/ahci_stats` and
  `/proc/inflight_stats` showing zero in flight rule out a lost completion. When a
  hang is inside a loop over data, log each datum to `/dev/klog` before touching it;
  that names the datum in one boot.
- A `SIGKILL` escalation in `edos-vm stop` was tried and refuted. Every harness calls
  `vm("stop")` under `check=True`, so turning a slow exit into a raised `SystemExit`
  failed `fs-regression --fat32` outright where plain `SIGTERM` printed "stopped" and
  carried on. Do not add it without first showing a guest that survives `SIGTERM`.
- To measure a box's rendered width, take `edos-vm shot`, walk the PNG's rows
  collecting contiguous runs of the box's background colour, and group runs by
  `(start_x, width)`. The single widest run is wrong when two boxes share a colour.
- Attach a disk with no partition table to exercise the partition-scan failure path
  (every image the tree builds carries a GPT):
  ```
  dd if=/dev/zero of=~/.cache/edos/blank.img bs=1M count=8
  scripts/edos-vm start --extra-disk ~/.cache/edos/blank.img
  ```
  The scan logs `GPT parsing failed on device 2: GPT header carries no signature,
  trying MBR` and the boot continues.

### Remeasure every count before quoting it

| count | how |
|---|---|
| syscalls | rows in `kernel/src/syscalls/table.rs`; `/proc/syscalls` publishes the same list |
| userspace programs | `members` in `programs/Cargo.toml` that carry a binary; `edos_lib`, `edos_render` and `edos_http` are libraries |
| binaries in `filesystem/bin` | `ls filesystem/bin \| wc -l`. It differs from the program count: `edos-edit` is packaged, not imaged; `gunzip` is a second `[[bin]]` of `gzip`; `ctest` comes from `libs/libgloss-edos` |
| Rust lines | `tokei -t=Rust` at the repo root (honours `.gitignore`); read the `Rust` row, not `(Total)` |
| kernel Rust | `tokei -t=Rust kernel/src` |
| commits | `git rev-list --count <rev>`, stating the rev |
| in-kernel tests | `make test AUDIODEV=none` and `make test-single AUDIODEV=none` |
| host unit tests | `make host-tests`, then sum the `test result: ok. N passed` lines; there is no single total |
| guest suites | `SUITES` in `scripts/guest-check` |
| `nvme-check` cases | the `case_*` functions in `scripts/nvme-check` |
| `unwrap()`/`expect()` | `grep -rIno --include='*.rs' -e '\.unwrap()' -e '\.expect(' kernel/src \| wc -l`; the leading dot excludes `#[expect(...)]` |

A matching count is not a matching inventory. Diff sets, not totals:

```bash
sed -n '/members = \[/,/\]/p' programs/Cargo.toml | grep -oE '"[^"]+"' | tr -d '"' | sort > members.txt
sed -n '/^| Area/,/^$/p' doc/USERSPACE-ROADMAP.md | grep -oE '`[a-z0-9_-]+`' | tr -d '`' | sort -u > tabled.txt
comm -3 members.txt tabled.txt   # only `gunzip` is expected
```

`SYS_ERRNO` is written `0x400`, so a decimal-only regex over syscall numbers misses it.
The site at `/usr/src/edos-web` carries the same counts (`src/pages/index.astro`
`TREE`, `src/content/docs/architecture.md`, `userspace.md`, `introduction.md`) and the
whole syscall table in `src/data/syscalls.ts`. Deleting dead code moves line figures
without touching any inventory, so reread them whenever the repo's counts move.

### `make host-tests` is the userspace suite

`scripts/host-tests`' header states the three mechanisms and the stale-binary trap.
Nothing discovers test modules: adding one means adding its crate to the `-p` list,
its lib to the `libs/` loop, or its file to `STANDALONE`. Crates under `libs/` are in
no cargo workspace, because they are shared with a kernel built for
`x86_64-unknown-none`, so nothing builds or tests them for the host unless that loop
names them; adding them to a workspace is not an option.

Coreutils that depend on `edos_lib` build only for `x86_64-unknown-edos`. The fast loop
for them is `cd programs && cargo +edos check --target x86_64-unknown-edos -p wc -p
sed ...` (about a second warm, no image rebuild); expected output must then be checked
in the guest. A tool with no `edos_lib` dependency compiles natively (`rustc +nightly
-O --edition 2024 -o /tmp/wc programs/wc/src/main.rs`) and runs on the host exactly as
the guest would; check its `Cargo.toml` first.

### `sccache` serves stale artifacts after the std fork is rebuilt

`CLAUDE.md` "Toolchain caveat" has the mechanism and the `rm -rf programs/target` plus
`SCCACHE_RECACHE=1 make programs` pair. `SCCACHE_RECACHE=1` alone leaves the poisoned
`target/` looking fresh. Three more toolchain traps:

- Before a full `./x install`, run `./x check library/std --target
  x86_64-unknown-edos` in `~/dev/rust`. It takes seconds and catches compile errors;
  only behavioural failures need the install loop.
- After bumping the `edos_rt` pin, `./x install` can rebuild nothing: bootstrap does
  not notice the lockfile change and reports success in seconds, and userspace keeps
  linking the old std. `touch library/std/src/lib.rs` first. A build that finishes far
  too quickly after a dependency bump has not done what was asked.
- `./x check library/std` can fail with hundreds of `E0514: found crate core compiled
  by an incompatible version of rustc` when stale rmeta sit under
  `build/x86_64-unknown-linux-gnu/stage1-std/x86_64-unknown-edos/`. Delete that one
  directory; it rebuilds in about 30 s. Do not run `./x clean`, which discards the
  whole build tree including the downloaded CI LLVM. The installed `+edos` toolchain
  lives under `install.prefix` in `bootstrap.toml`, outside `build/`.

### `edos_rt` and the std fork

The publish loop is in `CLAUDE.md` and `doc/rust-fork-rebase.md`; the allocator design
is `doc/design/allocators.md`.

- The `~/dev/edos_rt` clone can lag crates.io: releases have been published from a
  tree that never reached `github.com/edg-l/edos_rt`, and patching the clone then
  reverts them silently. Diff it against the published crate before editing:
  ```
  curl -sL -o rt.crate https://static.crates.io/crates/edos_rt/edos_rt-<version>.crate
  mkdir -p rt && tar xzf rt.crate -C rt --strip-components=1
  diff -ru rt/src ~/dev/edos_rt/src
  ```
  Use `static.crates.io`. The `crates.io/api/v1/.../download` form answers with a
  refusal as a 200 with a JSON body, which surfaces two commands later as `gzip:
  stdin: not in gzip format`.
- Test an `edos_rt` change through a `[patch.crates-io]` path override onto
  `~/dev/edos_rt` before publishing; a crates.io version cannot be withdrawn.
- Run `bench/allocstress` in the `edos_rt` repo before publishing. It builds the
  allocator against a shimmed `mmap` on the host and fails if the pool does not
  plateau, freeing everything does not return memory, an over-aligned large request
  comes back misaligned, the heap's tags and bins disagree, or cost tracks the live
  population. It has stopped compiling unnoticed before; check it builds.
- The inline syscall wrappers declare argument registers `inout(...) => _`, not
  `in(...)`: a syscall that parks resumes through the scheduler, so registers are not
  preserved.
- A type owning a kernel descriptor has four rules. `into_raw_fd`,
  `IntoInner<OwnedFd>` and `From<FileDesc> for OwnedFd` must `mem::forget(self)`. An
  explicit `close(self)` must not also close in `Drop` (`FileDesc::close` is a no-op
  that lets `Drop` do it). Dropping a pipe end changes when a read returns zero, and
  `edos-sh` pipelines, `sshd` and `edos-init` all depend on the parent closing the end
  it handed to the child (`stdtest`'s `Command::new("/bin/echo").output()` exercises
  it). Stdio holds no `FileDesc`, so nothing closes the terminal under a program.
- `strace -e openat,close,read <prog>` finds a descriptor leak: `openat` numbers that
  climb by one per call with no `close` between.

### Build traps

- make runs recipes under `/bin/sh`, which is dash on Debian: no brace expansion.
  `mkdir -p filesystem/{bin,dev}` creates one directory named `{bin,dev}` and succeeds.
  The `filesystem` rule uses `$(addprefix filesystem/,$(FILESYSTEM_DIRS))`.
- A file generated into `filesystem/` must depend on its generator and nothing else,
  as `$(WALLPAPERS): scripts/mkwallpaper.py` does. `filesystem/.manifest` records
  mtimes, so regenerating an unchanged file every build rebuilds both disk images
  every build.
- `make edos-x86_64.iso` re-invokes the kernel build without any `CARGO_FLAGS` given
  to an earlier kernel build, replacing an instrumented kernel with a plain one. Pass
  `CARGO_FLAGS` to the ISO target itself.
- `cargo check` or `cargo clippy --manifest-path kernel/Cargo.toml` from the repo root
  uses the root `rust-toolchain.toml` (plain `nightly`), not `kernel/rust-toolchain.toml`
  (a pinned nightly), and fails with `x86_64-0.15.4` not implementing
  `Step::forward_overflowing`, which does not look like a toolchain mismatch. Use
  `make -C kernel check` / `make -C kernel clippy`.
- Cargo finds `.cargo/config.toml` relative to the working directory, not to
  `--manifest-path`. `cargo +edos clippy --manifest-path programs/Cargo.toml` from the
  root loses `programs/.cargo/config.toml` (default target, rustflags). `cd programs`
  first, as `make clippy` and CI do.
- The version lives only in `kernel/Cargo.toml`. `/proc/version` renders it from
  `CARGO_PKG_VERSION`, and `uname` and the shell banner read that. No version literal
  anywhere else.
- `cargo --artifact-dir` hardlinks binaries into `filesystem/bin`, sharing an inode
  with `programs/target/x86_64-unknown-edos/`. `programs/Makefile` runs `objcopy
  --strip-debug` into a new file and moves it over, which breaks the link and keeps
  `target/`'s copy symbolised for `addr2line`. An in-place `strip` would destroy it;
  `--strip-all` would drop `.symtab`.
- `live-root.img` is sized at 1.4x `filesystem/` with a 64 MiB floor (`GNUmakefile`).
  With stripped binaries the floor tends to set the size, so the floor is the lever,
  bounded by what a live session must write.
- A `shipped = false` program (`pkg.toml`; currently `edos-edit`) is moved from
  `filesystem/bin` to `pkgstage/bin` by `programs/Makefile` after every build, and
  every image target depends on `programs` through `filesystem/.manifest`. Copying it
  back and running `make nvme-disk.img` undoes the copy. To stage one into the guest,
  copy it after the last program build and run the image recipe's `sgdisk` and
  `efs-mkfs --populate` by hand.
- `cargo test` in `tools/efs-fsck` runs `tools/efs-fsck/target/release/efs-fsck`
  (`tests/common/mod.rs::fsck_bin`), which only `make efs-fsck` rebuilds. Without it, a
  revert-and-watch-it-fail check passes both ways.
- `scripts/edos-vm start` rebuilds an image only when it is older than
  `filesystem/.manifest`, which a change to `tools/efs-mkfs` or `libs/efs-common` does
  not touch. The image make rules list those sources, so gates rebuild; a bare `start`
  does not. Run `make nvme-disk.img sata-disk.img` after changing either.
- `alloctest` never exits by design. Anything running the test binaries in sequence
  hangs on it.
- `sg` is also the ast-grep binary. Scripts wanting the group tool use `/usr/bin/sg`,
  as `scripts/edos-vm` does.

### Lints, warnings and dead code

- A file-level inner attribute (`#![expect(unused)]`, `#![allow(dead_code)]`)
  silences that lint for the module and every child, while errors still surface, so
  the file looks checked. Before trusting a clean gate on a file, add `let
  gate_probe_unused = 5;` to a function in it and confirm the build names it. Audit
  with `grep -rl '^#!\[expect\|^#!\[allow' kernel/src`. Only whole-spec register
  transcriptions (`hda/regs.rs`, `e1000e/regs.rs`, `ahci/fis.rs`) carry one.
- Kernel dead-code suppressions are `#[expect(dead_code, reason = ...)]` plus three
  `cfg_attr(not(feature = ...), allow(dead_code, ...))` for feature-only items
  (`util/ring.rs`, `thread/sched_prof.rs`, `thread/thread.rs`). Check every feature
  set before deleting: default, `sched-test`, `trace`, `sched-prof`.
- A suppression on a whole `impl` block covers every method in it, present and
  future. Put suppressions on items.
- A trait default method with no callers hides its overrides: deleting the trait
  method surfaces them, and only then do the fields they read show as unused.
- An item read only through a derived `Debug` counts as dead; so does a DMA buffer
  the hardware reaches by physical address (`UsbDevice::output_ctx`, whose address sits
  in the DCBAA). The uaccess fault-resume label is armed from `do_user_copy`'s inline
  assembly via `setup_fault_resume`, which no Rust caller names. A constant that looks
  dead is often a magic number open-coded elsewhere: grep for its value first.
- `pub` items in a library crate are invisible to the dead-code lint. So an
  `#[expect(dead_code)]` there is unfulfilled (delete it), and unused toolkit API is
  found by grepping for callers.
- Clippy and rustc replay cached diagnostics. `touch src/main.rs` between feature-set
  checks, before `cargo clippy --fix`, and before measuring a lint with `-W` or
  `--force-warn`; otherwise a warm tree answers 0 or "no change".
- `cargo clippy --fix` reverts the whole batch if any suggestion fails to build.
  `useless_format` rewrites `format!("literal")` to `"literal".to_string()` without the
  `alloc::string` import a `no_std` crate needs. Drive it one lint at a time: `-- -A
  clippy::all -W clippy::<lint>`.
- Suggestions that do not compile: `manual_memcpy` on a `#[repr(packed)]` field
  (`LfnEntry`) proposes `copy_from_slice`, which is E0793; assign the whole array. A
  `///` block separated from the next item by a blank line still documents that item;
  a module-level table belongs in `//!`.
- Measure a lint across the kernel before adopting it:
  ```
  cd kernel && touch src/main.rs
  cargo clippy --target x86_64-unknown-none -- -W clippy::<lint> 2>&1 | grep -cE '^\s+--> '
  ```
  `--message-format short` drops the lint name, so count the `-->` lines. The count is
  default-feature only; blocks inside macros that expand to nothing without a feature
  (`trace_event!` under `--features trace`) are seen only by `make -C kernel clippy`,
  which loops over every feature set.
- Count clippy findings by exit code under `-D warnings`, not by grepping
  `^warning:`: some messages start with a backtick, and a deny-level lint aborts before
  the rest are reported.
- `allow_attributes_without_reason` fires on `#[expect]` as well as `#[allow]`. Count
  reasonless suppressions with clippy, not grep: a multi-line `#[expect(\n dead_code,\n
  reason = "..."\n)]` defeats `grep -v reason`.
- `clippy::too_many_arguments` is on in both trees, with per-site `#[expect]` where it
  fires. `git grep -c too_many_arguments -- '*.rs'` is the reproducible count.
- `programs/` is a workspace; `libs/` and `tools/` are not. `[workspace.lints.clippy]`
  in `programs/Cargo.toml` reaches only members with `[lints] workspace = true`; a new
  program without that line is silently unlinted. `libs/` and `tools/` packages carry
  their own `[lints.clippy]`. `cargo clippy --all-targets` in `tools/efs-fsck` has
  findings no gate reads.
- Moving a program to edition 2024 stabilises let chains, and `collapsible_if` then
  flags `if cond { if let ... }` pairs.
- A sweep for plan vocabulary in comments ("Phase N") also hits real machine state:
  `thread/interrupt.rs` (the switch's two halves on different stacks),
  `drivers/usb/xhci/mod.rs` (config descriptor header then full fetch), and the NVMe
  completion-queue phase bit in `drivers/nvme/queue.rs` and `debug/lock_order.rs`.
  Leave those.

### Writing `// SAFETY:` comments

`doc/rust-style.md` has the rule and the lint. What each kind of code has to argue:

- MMIO `read_volatile`/`write_volatile`: the mapping that produced the pointer (for
  example BAR0, mapped in `NvmeController::new`), natural alignment inside it, and that
  `volatile` is there because the device changes the value.
- Port I/O: who else drives the port. `pci/config.rs` is the only user of 0xCF8/0xCFC,
  and `PCI_CONFIG_LOCK` keeps the address write and data access together, so the lock
  is load-bearing to soundness.
- Per-CPU state (`util/per_cpu.rs`, `sched()`, control-register writes in `fpu.rs`):
  argue migration, not validity. The pointee is `'static`; the risk is touching the CPU
  the thread left.
- DMA buffers: the load-bearing half is why the device is not touching the buffer now.
  Submit side, the command is not yet issued; completion side, the slot's `SACT` bit
  cleared or `wait_for_completion` returned; `e1000e`, the NIC owns only `[RDH, RDT)`.
  If that clause cannot be written, the code is wrong.
- Bring-up (`gdt::init_current_cpu`, `smp::ap_start`, `boot::kmain`): "nothing else
  exists yet", which stops being true if the function is ever called twice.
- `restore_fpu_state`: `FXRSTOR` raises #GP on a reserved `MXCSR` bit, so the image
  must come from `save_fpu_state` or `init_fpu_state`; that is why
  `FpuState::default` writes `MXCSR_DEFAULT`.
- uaccess call sites: the helpers null-check, `access_ok` and fault-trap the user
  side, so the comment argues the kernel side and names the bound: the source slice's
  own length, a clamp (`data.len().min(count)`), a `written + needed <= size` check,
  or a `T: Copy` of plain integer data.
- `read_unaligned` of a `repr(C)` on-disk struct: the loop or modulo bounds `offset +
  size_of::<T>()` by the buffer; `read_unaligned` needs no alignment; `efs-common`
  asserts the type's size, which rules out padding.
- A page-cache frame through `CachedPage::as_slice_mut` (takes `&self`): the page
  cache supplies no exclusion. Claim the pin, and name who supplies exclusion
  (`write_lock` for `block_page_cache`, the inode write lock for `zero_tail`).
- A trait impl method's `# Safety` says which part of the trait's contract this impl
  leans on (`GlobalAlloc::dealloc` may free on another CPU, since
  `try_percpu_dealloc` derives the size class from `layout` alone). An `extern "C"`
  entry point's contract is who enters it and how many times.

Placement and shape:

- The `// SAFETY:` goes on the line directly before the `unsafe` block's expression,
  not before the enclosing statement. A misplaced comment gets the same diagnostic as
  a missing one. For `match unsafe` after a `let`, it goes between `let x =` and
  `match` (as in `memory/fault.rs`). Converting `a && unsafe { .. }` to a `let` keeps
  short-circuiting; the same rewrite on `||` or an operand with a side effect needs
  checking.
- Each `unsafe impl` needs its own comment; one above a `Send`/`Sync` pair leaves the
  second flagged.
- Do not write the literal `SAFETY:` above a `mod` item: `unnecessary_safety_comment`
  reads it as a safety comment on a module and the build fails.
- Say a shared argument once: one comment above a group, then `// SAFETY: see the
  note above this group.` on each block. `cargo fmt` re-indents those one-liners, so
  grep them by text.
- Split a wide `unsafe` block down to the operations that are unsafe; a wide block
  hides which one the reader should check.
- Check whether the operation needs `unsafe` at all. `core::mem::zeroed()` on a
  `#[repr(C)]` integer struct is `#[derive(Default)]`.
- A helper that checks its bound is worth writing; one that only moves the block is
  not. `drivers/virtio/gpu.rs` has safe `write_at`, `zero_at` and `read_at` over
  `DmaBuffer`, each asserting the range, with `read_at`'s `T` bounded by the private
  `unsafe trait DeviceResponse`.
- Any index or length from a device or userspace that forms a pointer needs a bound in
  between. `Virtqueue::poll_used` (`drivers/virtio/queue.rs`) refuses a used-ring `id`
  at or above the queue size before `reclaim` walks the table with it. QEMU never sends
  one, so no test catches its absence.
- A safe fn carrying a `# Safety` section is ruled out, and clippy's
  `unnecessary_safety_doc` misses it on private items. Find it with `grep -B8 '#
  Safety' | grep -v 'unsafe fn'`.

## Scheduler, threads and synchronisation

### A `SeqCst` load is not a barrier, and that is where a has-waiters check goes wrong

`WaitQueue::has_waiters` opens with `fence(Ordering::SeqCst)`. A `SeqCst` load alone
lowers to a bare `mov` on x86; the barrier rides on `SeqCst` stores:

```asm
producer:                       waiter_publish:
  movb  $1, (%rdi)   ; store      xchgq %rsi, (%rdi)   ; publish, full barrier
  movq  (%rsi), %rax ; load       movzbl (%rdx), %eax  ; predicate
```

One side fenced and the other not is the store-buffer litmus: the producer sees no
waiter and skips the wake, the waiter sees no data and parks. LLVM lowers the fence to
`lock orl $0, (%rsp)`.

The two producers the fence protects, because their publication is a `Release` store
followed by a wake with no RMW or lock between: `PageFillHandle::finish_success` /
`finish_failed` (`kernel/src/fs/page_fill.rs`; a lost wake parks a reader on a finished
fill) and the writeback kthread's `flush_completed` store before waking `sync_done_wq`
(`kernel/src/fs/writeback.rs`; `wait_for_flush` is one un-looped `wait_until`, so a
lost wake hangs `sync`). Producers that publish under the waiter's lock (pipes) or with
a `compare_exchange` (`BlockIoHandle::complete`) are safe without it.

Recurrence tell: a producer whose publication is a plain or `Release` store, with no
RMW and no lock between it and a wake. `Scheduler::load` needs no such discipline: a
stale load read costs a slightly worse placement, not a lost wakeup.

### Waits on a peer must be killable

A killed thread dies at the syscall return boundary. A wait loop whose predicate only
a peer can satisfy never reaches it: the kill's wake fires, the predicate is still
false, and the thread parks again. `WaitQueue::wait_until_killable`
(`kernel/src/thread/waitqueue.rs`) also ends the park on the killed flag and returns
`WaitOutcome::Killed`; the caller returns `EINTR`. It is opt-in, because a wait is
abandonable only where the caller can abandon what it waited for: page fills and
journal commits keep parking. Users: `accept`, TCP and UDP socket read, pipe read and
write, pty-slave read and write. `sys_waitpid` carries the same check inline.

A pending stop is not handled: `SIGTSTP` on a blocked call needs restart semantics the
kernel lacks, so Ctrl+Z on a blocked `accept` does nothing until the call returns.

Recurrence tell: a syscall that parks on a peer's action, and a process that survives
`kill -9`.

### Every park consumes a wake token, so a one-shot park is a latent no-op

Every wake that ends a sleep or a park leaves a wake-pending token that survives into
the thread's next park, and `transition_park_while` consumes it and declines to park.
Any single, non-looping `thread_park_while` call is therefore a bug: loop on the real
condition around the park, as `stop_if_signalled` does. Re-parking is safe only where
the caller is enrolled on no wait queue.

A syscall that sleeps in a loop against an absolute deadline (`sys_nanosleep`) must
call `stop_if_signalled` as well as `exit_if_killed` inside the loop, or a kill gets
through and a stop does not. Suspended time counts against the deadline.

### The thread-exit path must not allocate or take a lock

It can run with interrupts disabled (see the comment on `reaper_enqueue` in
`kernel/src/thread/scheduler.rs`). Parentage bookkeeping runs in the reaper, and
`record_thread_exit` takes the parent from the dying thread the caller already holds. A
registry walk plus two `Vec` allocations on the exit path once showed up as a `make
test` timeout, not a panic. Anything added to thread exit: assume no allocation and no
locks, then run `make test`. `/proc/processes` prints `pending exit statuses`, which
stays flat across spawns when orphaned statuses are dropped correctly.

### Every default signal action goes through `apply_default_action`

`apply_default_action` (`kernel/src/thread/thread.rs`) is called from both the send
path and `deliver_unblocked_signals`. A second copy of that match once cleared
`stop_requested` but not `stopped` on `SIGCONT`, leaving a runnable thread that `ps`
reported `Stopped`. Do not reintroduce a per-caller copy. `SIGCONT` clears `stopped` at
delivery, not when the target next runs; otherwise `fg`'s immediate wait sees it still
stopped.

Signal frame facts (`kernel/src/syscalls/sigframe.rs`; `programs/sigtest` is the test):

- `SigFrame` holds the whole interrupted `SyscallContext`, the old blocked mask and a
  magic word, written below the red zone so `rsp+8` is 16-aligned at handler entry.
- `sigreturn` checks the magic and masks rflags before loading. It restores `rip`,
  `rsp` and `rflags` from user-writable memory, so dropping either check is a
  privilege escalation.
- The saved `rax` is the interrupted syscall's return value, so a handler running
  between a call finishing and userspace seeing its result is invisible to the code.
- Delivery happens only at syscall return (`deliver_pending_handler`). Extending it to
  the tick path means building the frame from a `CpuContext`.
- A handled signal does not also take its default action: `kill_process_with_signal`
  returns early when a handler is installed.

### Pipes and terminals are bounded at 64 KiB

`PIPE_CAPACITY` is 64 KiB (`kernel/src/thread/pipe.rs`). A write that does not fit
parks, killably, until a read frees room or the last reader leaves. A write of at most
`PIPE_BUF` (4096) waits for room for all of it, so two writers never interleave a small
message. A reader leaving mid-write returns the bytes already transferred, not
`EPIPE`.

A bounded pipe deadlocks any writer that fills it before starting its reader.
`edos-sh` feeds a heredoc from a thread that owns the write end (`heredoc_pipe`);
feeding it inline hangs the shell on a heredoc over 64 KiB. Any code that writes to a
pipe it will later read from itself needs the same shape. A program capturing a child's
output reads the pipe dry before `waitpid`, for the same reason. Guest checks: `yes |
head -3` terminates; `seq 1 200000 | wc -l` reports 200000; a 20000-line heredoc
completes.

`PTY_OUTPUT_CAPACITY` is 64 KiB (`kernel/src/thread/pty.rs`), with a killable wait in
`sys_write` and poll's writable bit following free space. The bound is on stored bytes:
`ONLCR` stores one newline as two, so `write_output` takes a `room` argument and never
splits a CR from its LF. The input side discards rather than waits:
`PTY_INPUT_CAPACITY` is 4096 (POSIX `MAX_INPUT`), with Ctrl-C, Ctrl-Z and Ctrl-D exempt
so a full queue stays recoverable. A write to a slave whose last master closed is
`EIO`, not `SIGPIPE`. `iotest` test 21 fills a pty nobody reads and checks the bound;
setting `PTY_OUTPUT_CAPACITY` to `usize::MAX` turns it red.

### The PTY translates newlines, and only a remote terminal shows it

`LineDiscipline` (`kernel/src/thread/pty.rs`) carries `opost` (POSIX `OPOST` +
`ONLCR`): on in canonical mode, off in raw. The `edos_render` terminal widget treats
`\n` as CRLF, so a bare-LF bug is invisible locally and shows as a staircase over SSH.
Check line endings over `ssh`. `edos-sh` is raw only inside `read_line` (the `RawMode`
guard); its line editor emits CRLF explicitly.

### The clock

`Instant` holds nanoseconds (`kernel/src/timer.rs`), so values stay comparable across
a change of clock source. The TSC is the source only when `invariant_tsc` reports
`CPUID.80000007H:EDX[8]`, and each AP is checked against the HPET at bring-up
(`verify_tsc_sync`). `clocksource=hpet` on the command line forces the HPET.

QEMU advertises invariant TSC only when asked, so every run target and `scripts/edos-vm`
pass `-cpu ...,+invtsc`. A new QEMU invocation without it silently falls back to the
HPET, whose read exits to QEMU's userspace: 6361 ns per read against 16 ns for
`rdtsc`, measured. Under TCG the bit is absent and the HPET is kept, which is correct
because TCG's TSC counts instructions.

`set_apic_timer` (`kernel/src/apic/mod.rs`) raises every duration to
`MIN_TIMER_INTERVAL`: a count of 0 stops the one-shot permanently, and one tick at Div1
fires before the arming handler returns. A loop that reads the clock gets no backoff
from the read on the TSC, where it would from an HPET exit; suspect that first if a spin
loop misbehaves.

The wall clock reads the RTC once at one-second resolution and answers
`clock_gettime` from that pin plus the monotonic clock. `SYS_CLOCK_SETTIME` steps it
through the atomic `WALL_CLOCK_OFFSET_NS`; the pin and the monotonic counter are
untouched, so durations never jump. `programs/sntp` (RFC 4330) checks mode 4, stratum
1..15, and that originate equals the transmit timestamp it sent.

### `stall-dump` and the heartbeat threads

`kernel/src/debug/stall.rs` (feature `stall-dump`) declares a stall when its switch
counter stands still for `STALL_MS` (4000 ms), prints every thread with its state and
a backtrace once, then `stall: still nothing, switches=N` per later window.

Five kthreads wake on a timer forever and would keep the counter moving on a
deadlocked machine: `tcp-retransmit`, `nvme_watchdog`, `ahci_watchdog`,
`block_writeback`, `journal_committer`. Each calls `stall::mark_heartbeat()` on entry,
and `note_switch` skips a heartbeat thread's switch. A new periodic housekeeping
kthread whose normal answer is "nothing to do" must call `mark_heartbeat` too, or the
detector goes blind. The exclusion covers only the heartbeat thread's own switch.

`make stall-check` proves the detector fires. A desktop boot never dumps: the taskbar
clock alone is work every 4 s. `scripts/wedge-probe` is the first instrument for a
suspected wedge (QMP counters, no rebuild); `stall-dump` is the next when the question
is which thread waits on what.

### A parked thread is not load

The `load-parked-is-not-load` sched-test case (`kernel/src/thread/sched_test.rs`)
compares two CPUs whose contents the test controls: 32 threads parked on one against
`LOAD_SPINNERS` running on the other, asserting the parked CPU reports less
`Scheduler::load` and wins a placement restricted to those two. Its first form asked
whether the parked CPU won a placement against the whole machine; that depended on
every other CPU, lost correctly to any idle one, and failed two runs in three with the
fix in. Write an assertion over state the test owns, not over the machine.

### Re-read every predicate after a mechanical lock rewrite

Wrapping `wait_until(|| !self.queue.lock().is_empty())` in `ranked_lock!` once lost the
negation. The boot hung right after the root mount with the serial log stopping: the
FS mailbox thread waited on an inverted predicate. It looks like a deadlock in
whatever ran last.

## Memory

### Per-process memory in procfs

`/proc/processes` has an RSS column; `/proc/<tid>/status` has `VM Size` and `Resident`.
Resident is counted from the page tables at read time (`MemoryManager::resident_bytes`
/ `resident_bytes_in` in `kernel/src/memory/mapper.rs`), not kept as a counter: pages
enter and leave through too many sites for a counter not to drift. Lock order for the
walk: `vmas` (70) then the per-process mm (80).

Trap: the reaper calls `Thread::free` before removing the thread from the registry, and
procfs snapshots the registry first, so a reader can reach a `MemoryManager` whose PML4
frame is back in the allocator. `Thread::free` calls `release_page_tables()` under the
mm lock, and `resident_bytes` returns 0 once `released` is set. Any new reader of the
raw page-table frame must check the same flag.

### `USER_VA_END` cannot go in a `VirtAddr`

`USER_VA_END` (`0x0000_8000_0000_0000`, `kernel/src/memory/vma.rs`) is the lowest
non-canonical address, and `VirtAddr::new` panics on it. A half-open range over the
whole user half uses raw `u64`, as `resident_bytes_in` does. A violation panics the
first time something reads `/proc/processes` (the panel, seconds after boot), not at
boot.

### VMA protection is recorded as requested

The kernel records protection exactly as the caller asked, and the loader maps ELF
`p_flags` one for one. Nothing reads `VmaProt::READ` except `pmap`/`maps` rendering;
`memory/fault.rs` checks only `WRITE` and `EXEC`. An odd triple such as `-w-p` in
`pmap` is the caller's request, not a kernel bug. There is no `brk`; `heap_break` is
only the starting address for `next_mmap_addr`.

### `mmap` answers a `NonNull`, and `MAP_PHYSICAL` has its own wrapper

`edos_lib::mem::mmap` returns `Result<NonNull<u8>, Errno>`, because the syscall has two
non-mapping answers (a negated errno and null). A physical mapping is
`mem::mmap_physical`, which sets `MAP_PHYSICAL` itself: the kernel reads `r8` as a
physical address only under that flag and as a file descriptor otherwise, so the flag
and the argument must never be set independently.

## Syscalls and the ABI

### Validate every argument, and check a declared length yourself

A length or maximum a caller passes is a claim about a buffer it owns, and
`try_copy_to_user` checks only the bytes written. A syscall taking a declared size
checks the declared size: `sys_getcwd` and `sys_netinfo` call `access_ok(buf, len)`,
and `sys_window_list` checks `max * size_of::<WindowListEntry>()` with `checked_mul`
before taking the registry lock.

Validate every argument before any zero-length short-circuit. `read(9999, p, 0)` must
be `EBADF`, because a zero-length transfer is how userspace probes a descriptor:
`read`/`write`/`readv`/`writev` check with `fd_is_open`, and
`pread`/`pwrite`/`sendto`/`recvfrom` return for `count == 0` only after resolving the
descriptor. The null check stays after the length check: `read(fd, NULL, 0)` is 0.
`sys_getrandom` rejects unknown `flags` and `sys_futex_wake` calls `access_ok` before
their `count == 0` returns. `iotest` test 20 checks the descriptor case.

`open` refuses unknown flag bits with `EINVAL` (`OPEN_FLAGS_SUPPORTED` in
`syscalls/io.rs`), deliberately unlike Linux: silently dropping `O_EXCL` or
`O_DIRECTORY` would return a descriptor with the wrong semantics. Adding an open flag
means adding its bit there, or every caller gets `EINVAL`. `iotest /var` exercises the
mask through std.

`O_TRUNC` truncates only a regular file (`open_resolved` checks `FileKind::File`), per
POSIX; devfs has no `truncate`.

### Reading `syscallfuzz` output

`programs/syscallfuzz` draws some arguments from plausible values on purpose (one
pointer in four is a valid 4096-byte scratch buffer; scalar sets lead with 0 and 1),
or the kernel's own checks would short-circuit every case. A success is reported only
when at least one argument was poison; the rest are tallied as `benign`. Rows that are
not defects: calls with no failure return (`isatty`), count queries with a zero
maximum (`list_dir`, `list_mounts`, `list_partitions`, `window_list`,
`clock_gettime`), and `futex_wake` with a valid address and a huge count. Read a row's
arguments before assuming it is last run's finding.

### A failing syscall returns a negated errno

The convention is in `CLAUDE.md`. Beyond it:

- `Errno` uses Linux `asm-generic/errno.h` numbering. One macro in
  `kernel/src/syscalls/mod.rs` generates the enum, `ALL_ERRNOS` and `name()`;
  `edos_rt` has its own copy of the list, and its `from_raw` cannot be a `transmute`
  since the values are sparse.
- The sentinel test lives at several layers, and each must test the window, never
  `== -1`: `edos_rt`'s `cvt`, the std fork's `cvt` in
  `library/std/src/sys/pal/edos/common.rs`, `sys/stdio/edos.rs`, and raw-syscall
  callers in `edos_render`, `edos-sh` and `syscallfuzz`. Changing the convention needs
  one `./x install` per layer.
- Errno is not cleared on success, by decision: `edos_rt::sys_result` reads it only
  after a call reports an error, as POSIX means it, and clearing it would add a
  `current_thread_info().lock()` to every successful syscall. Do not re-propose it.
- Bisect a regression across these changes by checking out whole commits, kernel and
  userspace together.

### The syscall table is one list

`kernel/src/syscalls/table.rs` holds one list, each entry `number, "name", function,
(kind: type, ...)`, passed to a macro the caller names. `syscall_rows!` builds the
`SyscallInfo` array `/proc/syscalls` publishes; `syscall_arms!` in `mod.rs` builds the
whole `dispatch` function, because a `macro_rules!` invocation cannot emit match arms
in place. Arguments come off `rdi, rsi, rdx, r10, r8, r9` through one `FromReg` impl
per type, never `as` at a call site. Bodies return `Result<u64, Errno>`. `sys_sync` is
the syscall; `io::sync_all` is the work, for callers such as `power::quiesce`.

For a conversion across many syscalls: regex the repeating shapes, change the
signature, let `cargo check --message-format json` enumerate the leftovers, and run
`make guest-check` per file group, since a syscall regression is not a compile error.

### A userspace wrapper passes every argument its syscall reads

`SYS_IOCTL` reads five (`fd, request, arg, arg_len, flags`). Through `syscall3`, `r10`
and `r8` carry whatever the caller left there, and a nonzero length with a read or
write flag set sends `sys_ioctl` down its copy-in path, where `arg == 0` returns
`EFAULT`. The failure depends on register contents, so one call site succeeds or fails
according to what ran before it. `strace -e ioctl` shows the registers the kernel
received.

### Two path front ends

User paths enter through `copy_user_path` (NUL-terminated) and `copy_user_path_len`
(counted) in `kernel/src/syscalls/mod.rs`, filling a caller-owned stack `PathBuf` so
path syscalls never allocate. `syscalls/fs.rs` adds only resolution policy
(`read_user_path`, `read_user_path_with_len`, `read_user_path_at`). Do not fold the
other copy helpers into these: `copy_in`/`copy_out` move counted bytes through the heap
so a caller copies before taking a lock, and `read_user_str`/`copy_user_c_string`
return owned values that are not paths.

### `/proc/<tid>/fd` is a table, not a directory

It is `FD TYPE MODE POS NAME`, NAME to end of line, because pipes, PTYs and sockets
have no path. The bracketed number is `Arc::as_ptr` of the shared object, which pairs
pipe ends and PTY sides across processes. `Procfs::render_fds` clones the table handle
out from under the thread-info `IrqSpinlock` before taking the `BlockingMutex` table,
and clones descriptors out before rendering because describing a socket takes the
socket lock. A `FileDescriptor` clone does not touch open counts (only `close` does).
Path-based procfs reads may park: `vfs::read` drops the inode guard before
`fs.read_bytes` when the inode is `None`, which is every procfs file. The kernel's unit
is the thread, so `lsof` lists a multi-threaded process once per thread.

### The ACPI handler is never exercised

The kernel does not run the AML interpreter: `power.rs` maps the DSDT and scans it by
hand. Every hook in the `acpi::Handler` impl (`kernel/src/acpi/handler.rs`) is
unexercised on any boot, so a change there is proven by review alone. A non-zero
segment group or a config offset above 0xFF needs MCFG, which the kernel does not map:
such a read answers all-ones and a write is dropped with one log line. `stall` spins
and `sleep` parks (ACPI 6.5 §5.5.2.4.1). AML mutexes are the fixed 128-entry
`AML_MUTEXES` table; a handle past it fails the method rather than the boot, and
`release` ignores a caller that is not the recorded owner.

## Storage, filesystems and the journal

### `sync` and `/proc/journal_stats`

`sys_sync` (`kernel/src/syscalls/io.rs`) loops commit, flush, `advance_tail` to a fixed
point (`Journal::needs_sync_round`, which counts the open transaction), at most
`SYNC_MAX_ROUNDS` (8), and logs `journal still pending after 8 rounds` when it gives
up. `/proc/journal_stats` shows `active`, `sealed`, `pending`, `tracked`; after a plain
`sync` all read 0. `pending` stuck non-zero while `tracked` is 0 means transactions
committed and checkpointed but not retired: an `advance_tail` bound bug.

A wait with a 30 s timeout on `commit_wq` (`force_commit_and_wait`) turns a lost wake
into a 30 s stall; eight rounds of that is 240 s. A multi-minute silent stall in
`sync`/`fsync` points there first.

### When writes are not durable

Checklist for an install-and-reboot or any "writes not durable" symptom (the last one
was `doc/bugs/2026-08-19-sync-returned-before-the-extents-were-committed.md`):

- Read `failed_sync_passes` in `/proc/block_cache` on the writing guest first. A forced
  writeback pass that did not write every dirty page logs `writeback: forced pass for
  request N did not write every dirty page; sync is returning without full durability`
  and bumps it. `SYS_SYNC` returns no error, so `edos-install` reads the counter either
  side of the sync, then issues `BLOCK_IOCTL_FLUSH` after it (a flush before the sync
  empties a cache that has not yet received the sync's writes).
- Refuted, do not re-check: `sys_sync` missing file pages (`BlockPageCache::sync_all`
  runs `flush_dirty_inodes` between two block-cache drains); writeback submitting
  without waiting (`write_batch` waits every handle); a stale journal tail
  (`write_journal_sb` ends in `block_write_fua`); QEMU losing writes on kill
  (`nvme-blank.img` is raw); deferred orphan eviction (an install unlinks nothing).
- Do not add a settle delay to `scripts/nvme-check` after `edos-install` exits. It
  would hide a real defect: a user who installs and reboots promptly gets the same
  disk.
- A repro starts from `make fresh-nvme-blank`. After one install, `nvme-blank.img`
  carries a bootable ESP and QEMU boots it ahead of the CD. Plain `make nvme-blank.img`
  is a file target and does nothing when the file exists.
- Check an installed image with `tools/efs-fsck/target/release/efs-fsck -n -v
  --partition-offset <bytes> <image>`. Directory blocks current against a stale inode
  table and bitmaps is metadata left in an uncommitted transaction.
- `efs-fsck` findings on a power-cut image are unreliable until the journal is
  replayed: later phases check home blocks the ring may hold newer copies of. Run
  `--repair` (replays first) and re-check, or `shutdown` in the guest instead of
  `edos-vm stop`.

### Orphan eviction waits for writeback on EFS

An unlinked file with dirty pages is not evicted when its last descriptor closes.
`DIRTY_INODES` (`kernel/src/fs/vfs.rs`) holds a strong `Arc<VfsInode>` so a close
cannot free pages that never reached disk, so `VfsInode::drop` and
`fs::evict::post_evict` fire only after `writeback_thread` releases it, up to one 5 s
period later. Measured: `evicttest /tmp` passes in 0.7 s, `evicttest /var` in 5.0 s;
`evicttest` polls on a 20 s deadline (`DRAIN_TIMEOUT`), and any orphan-reclamation test
on EFS needs the same allowance. `/proc/evict_stats` separates `posted_count`,
`drain_count` and `error_count`.

Writing back an orphan's dirty pages is wasted I/O, but `remove_file` keeps them on
purpose: live mappings keep reading and writing through them. Do not drop them at
`mark_orphan` without a measurement and a plan for mapped orphans.

### A filesystem cannot resolve a symbolic link

A filesystem sees a mount-relative path and cannot know the mount table, so it never
resolves a symlink target. Its walk stops at the first link it must follow and returns
`Error::LinkEscape`. The VFS asks `FileSystem::link_escape`, which answers
`LinkEscape::Absolute` or `LinkEscape::AboveMount` (see `fs::splice_symlink` in
`kernel/src/fs/mod.rs`), builds an absolute path and restarts from the VFS root. The hop
cap lives in the VFS and counts hops across mounts.

- Escalation is error-driven: `fs::api::with_links` runs the operation and redirects
  only on `LinkEscape`, so a path with no links costs one walk.
- Follow versus nofollow travels as `LinkMode` from the API layer. `unlink`,
  `readlink`, `symlink` and `rename` leave the final component alone; everything else
  follows. `rename` settles both paths with `resolve_links` first.
- `open` caches the path on the descriptor, so it takes the resolved one from
  `file_info_resolved`.
- Anything reaching the VFS outside `fs::api`'s retry loop sees `LinkEscape` as a plain
  error. The ELF loader's door is `fs::api::resolve_inode`, which stays inside the
  loop. A wrong errno (ENOEXEC, ELOOP, EIO) on a path `cat` reads fine is the
  signature of a caller outside the loop.
- Standing hazard: `link_escape` is asked with the API's `LinkMode`, not the mode the
  filesystem walked with. A filesystem operation that follows a final component the
  API says to leave alone surfaces as EIO. Any new filesystem operation must walk with
  the mode `fs::api` passes for it.
- `iotest` test 10 covers exec through a link, a linked directory, rename of a link,
  `rmdir` refusing a link, and a two-link cycle.

### FAT12 entries can straddle a sector

A FAT12 entry is 1.5 bytes, so its 16-bit window starts at `cluster * 3 / 2`; for one
cluster in every 342 that offset is 511 and the entry spans two sectors. `sector_span`
(`kernel/src/fs/fat32/mod.rs`) gives how many sectors a window needs, and
`get_fat_entry`, `set_fat_value` and `alloc_cluster` use it. FAT16 and FAT32 never
straddle.

A FAT12 test disk: format the partition standalone, then place it (`mkfs.fat --offset`
inside a partitioned image did not mount):

```
dd if=/dev/zero of=fat12.part bs=1M count=2
mkfs.fat -F 12 -S 512 -s 1 -n FAT12TEST fat12.part
mcopy -i fat12.part big.txt ::/BIG.TXT
dd if=/dev/zero of=fat12disk.img bs=1M count=4
printf 'label: dos\nstart=2048, size=4096, type=1\n' | sfdisk fat12disk.img
dd if=fat12.part of=fat12disk.img bs=512 seek=2048 conv=notrunc
scripts/edos-vm start --extra-disk /abs/path/fat12disk.img
```

FAT12 caps at 4084 clusters, so 16 MiB at one sector per cluster is too large; 2 MiB
works. A raw unpartitioned image gets no `/dev` node. Mount with `mount <dev> 0 /mnt
fat32` (the driver serves all three widths); check the device index first, since both
NVMe and SATA disks are attached.

### FAT12/16 roots are a fixed region, not a cluster chain

The root is addressed as cluster 0. `Fatfs::root_dir_cluster`, `is_fixed_root` and
`dir_entry_region` (`kernel/src/fs/fat32/traverse.rs`) name that case, and every
directory write goes through `dir_entry_region`. Never pass a directory cluster to
`cluster_to_lba`: its `cluster < 2` guard returns the partition start, the boot sector,
and a directory entry written there is silent corruption. A full fixed root returns
`NoSpace`.

In `fat32/write.rs`: delete a long-name sequence before marking the short entry `0xE5`
(`delete_long_name_sequence` skips entries already marked, so reversing orphans the
LFN entries), and every FAT write goes through `write_fat_sectors`, which mirrors to
`backup_fat_lba`. Host `fsck.fat -n` on an image the guest wrote catches these; guest
`ls` and exit codes do not.

`determine_fat_variant` refuses any volume whose `bytes_per_sector` is not 512, and
`traverse.rs` computes FAT sectors with a literal `/ 512` while `write.rs` uses
`boot_info.bytes_per_sector`. Lifting the refusal requires fixing `traverse.rs` first.

### Filesystem errors carry their cause

`fs::Error` has `NoMemory`, `NotEmpty`, `BadAddress`, `Unsupported`, and
`Error::Block(#[from] BlockError)`. A new error path picks the variant that names the
cause; `map_err(|_| Error::IoError)` flattens a `BlockError` the syscall layer could
have reported. `FileSystem` trait defaults answer `Unsupported`. `gpt::parse_gpt` and
`mbr::parse_mbr` answer `gpt::PartitionError`, separating structural refusals
(`Signature`, `Truncated`, `Malformed`) from I/O ones. `loader::load_elf` is
`parse_image` then `map_image`, so every parse error fails before anything is mapped.

### One extent-tree encoder

`efs_common::build_extent_tree` (`libs/efs-common/src/extent.rs`) encodes an inode's
extent node for both the kernel EFS driver and `efs-mkfs`; they differ only in
`emit_leaf`. Keep on-disk allocation rules in `efs-common`: two implementations is how
an image `efs-fsck` calls clean fails to mount.

### `DmaPool` does not zero a recycled buffer

`DmaPool::allocate_sized` (`kernel/src/drivers/dma.rs`) serves AHCI per-command buffers
up to 2 MiB, and a memset per pop is a storage regression. A parser that reads a fixed
size out of such a buffer, instead of the byte count the device reports, returns the
previous owner's bytes after a short transfer. This bit xHCI descriptors, USB mass
storage and AHCI ATAPI. Any new driver reading from `DmaPool` bounds its parse by the
transferred count.

### A `/dev` descriptor bypasses the VFS read path

`sys_read` and `sys_pread` try `devfs::try_lookup_from_full_path` then
`device.read_to_user` before the VFS. An optimisation in `vfs::read_to_user` never
reaches `/dev/ram0` or any other device node. The kernel allocator has no
`alloc_zeroed` override, so `vec![0u8; n]` is a real memset.

### `fsbench` output scrolls its own verdict away

The `KERNEL COUNTERS` block is longer than the guest terminal, so the `verify:` line
scrolls away. Redirect and grep: `fsbench write -n 8 /var > /var/w.txt`, then `grep
verify /var/w.txt`.

## NVMe

### What a zero NVMe timeout can and cannot prove

`nvme_timeout_ms=0` declares every command hung the instant it is issued, so the reset
path runs immediately and repeatedly. The watchdog sleeps
`WATCHDOG_TICK.min(timeout.max(1 ms))` (`drivers/nvme/watchdog.rs`), so it sweeps
every millisecond against a reset that takes two or three, and the controller is in
reset most of the time: about 930 resets a second, measured. With the block-layer
retry the boot mounts root, starts init and reports no failed I/O, but the desktop is
not responsive and the taskbar sometimes never appears. No correctness fix changes
that. `nvme_timeout_ms=1` exercises nothing: commands complete in about 100 us under
KVM. Zero is the only setting that reaches the path, which is why `case_watchdog` in
`scripts/nvme-check` asserts init runs, the watchdog fires, the reset completes and no
I/O fails, and not that the desktop comes up.

Reading that boot's log: a kernel `log!` line may be missing although the event
happened, because about a thousand watchdog lines a second evict it from the ring
before the klogger drains it, so `Root filesystem mounted` is often absent. Userspace
writes serial directly, so `init: pid` is the liveness marker.

### Block-device ids, the probe barrier, and the id ioctl

Block-device id ranges: AHCI `0..1000`, USB mass storage `1000..2000`, ramdisk
`2000..3000`, NVMe `3000..`. `devfs::block::device_name` imports the NVMe base and
stride from `drivers::nvme`.

`NVME_PROBE_DONE` must be signalled on every path out of the probe kthread, including a
machine with no controller. `fs_main_thread` waits on `nvme::api::wait_probe_complete()`
because the boot-time `block_io::list()` scan runs once, so a skipped signal hangs boot
on every AHCI-only machine. `NVME_NAMESPACES` is published before the signal.

A `/dev` name cannot be parsed back into a device id: `sd*` letters continue from the
AHCI count into USB storage, and an NVMe name omits the 3000 base. A program needing the
id opens the node and asks with `BLOCK_IOCTL_DEVICE_ID` (`fs/devfs/block.rs`), as
`edos-install` does.

### The default QEMU NVMe device splits every large run

QEMU's default `-device nvme` reports MDTS 512 KiB, and the filesystem batches up to
248 pages (992 KiB, from AHCI's PRDT), so every large run on an NVMe root is split. The
splitter runs on every boot: `split_requests` in `/proc/nvme_stats` is nonzero after a
desktop boot. `SplitOp::parts_done` (`drivers/nvme/cancel_op.rs`) records the first
error and completes the handle once, from the last part, because the parts still hold
PRP descriptors into the caller's buffer.

## Networking

### Loopback and the source address

`NetStack::source_ip_for(dst)` (`kernel/src/net/stack.rs`) is the only place that
decides a packet's source address; `send_ip_inner`, `send_udp` and `sys_connect` call
it. Loopback rewrites the source to `127.0.0.1`, so a transport computing its
pseudo-header checksum or connection key from `stack.local_ip` breaks loopback
silently. A new transport calls `source_ip_for`. The passive side keys a connection by
`ip_hdr.dst_addr`.

Non-blocking `connect` returns `EINPROGRESS`; `poll` reports writable when the
handshake resolves; `SO_ERROR` carries the outcome once; a second `connect` answers
`EALREADY` / `EISCONN` / the failure. Over loopback `EINPROGRESS` is never observable,
because loopback delivers inside the sending syscall. `programs/socktest` covers the
contract plus one connect to an address nothing answers.

Regression check for a leaked handshake: run `socktest`, then `netstat` immediately. A
surviving `SYN_SENT` row is the defect (`TcpConnection::abort`, called by `sys_close`
when `build_fin` returns `None`).

### ARP holds one packet per unresolved target

`ArpCache` holds one pending packet per unresolved target (`queue_pending_tx` /
`take_pending_tx`, `kernel/src/net/arp.rs`, RFC 1122 §2.3.2.2), flushed when the reply
arrives; newest wins, capped at 16 targets. `send_ip` returns `Ok(())` for a packet not
yet on the wire, and no caller retries on ARP. A cold-cache ping includes resolution in
its RTT. A stranded `SYN_RECV` with Send-Q 1 on the first connection after boot is the
signature of a packet dropped before the neighbour resolved. A UDP send to an
unreachable address on the guest's own subnet fails at once: no ARP reply, nothing
transmitted.

### Socket address lengths are value-result

`addr_len` on `recvfrom`, `accept`, `getsockname` and `getpeername` is capacity in,
real length out, copy bounded by capacity; all go through `write_sockaddr_out`
(`kernel/src/syscalls/net.rs`). `edos_rt` and the std fork initialise it to
`size_of::<SockAddrIn>()`; a new caller must too, since the kernel reads the field.
`sys_recvfrom` implements `MSG_PEEK`, `MSG_TRUNC` and `MSG_DONTWAIT` (`RECV_FLAGS`) and
refuses other bits; `sys_sendto` accepts only `MSG_DONTWAIT`. `socktest` checks these
against a real DNS reply, so it needs QEMU's resolver at 10.0.2.3:53. DHCP keeps its own
`IP_ID` counter in `net/dhcp.rs` because it runs before the stack has an address.

### `/proc/sockets`

`/proc/sockets` lists every `NetStack.tcp_connections` entry, then every `PORT_TABLE`
binding without a connection, as `PROTO RECVQ SENDQ LOCAL FOREIGN STATE` (`SENDQ` is
`snd_nxt - snd_una`). It is not derivable from `/proc/<tid>/fd`, because a connection
outlives its descriptor. A bound TCP socket that has a `tcp_conn` is skipped, or
established connections appear twice. Both tables are snapshotted and released before
any socket (260) or connection (270) lock; holding `NET_STACK` or `PORT_TABLE` across
them is legal by rank but parks the stack behind one `cat`. There is no kernel routing
table; `netstat -r` reconstructs routes from `/proc/net`.

### TCP writes and half-close from userspace

A TCP write returns 0 when the send window is full; use `edos_lib::net::send_all`,
which retries, instead of treating 0 as failure. End of input half-closes with
`edos_lib::net::shutdown` (`SYS_SHUTDOWN`), so a reply still arrives. `nc` has no UDP
mode. `httpd` answers one request per connection (`Connection: close`), so an idle
client holds a thread until it leaves. A background job that reads stdin (`nc -l 23 &`)
still receives what is typed at the prompt; use `tcpecho -p 23 -q &` to keep typing.

Listener post-mortem: `doc/bugs/2026-08-12-a-listener-unbound-by-its-own-connections.md`.

## Window system and GUI

### A window is invisible until its client paints

A window is created unmapped (`visible: false` in `kernel/src/window/registry.rs`); the
client maps it with `Window::show()`. No buffer is published until the first
`swap_buffers`. A mapped window with no buffer composites as its themed ground, so
mapping before painting costs one frame of empty window. `Window::resize` publishes an
unpainted buffer on purpose: the old pair is freed immediately after, and the
alternative leaves the compositor holding a freed shm id.

### Keys arrive as keycodes, never as characters

The kernel never sends a `Character` window event: `WindowEvent::character` exists in
`libs/window-abi` and nothing in `kernel/src/window/` constructs it. Clients get
`KeyPress`/`KeyRelease`; map with `edos_lib::keymap::{update_modifiers, map_keycode}`.
A program written against `event.character()` runs, draws, and ignores every key.

`Widget::on_key` receives a `pc_keyboard` `KeyCode` discriminant, the same space as
`edos_render`'s `keycode::` constants, not an AT set-1 scancode. `grep -rn 'scancode
==' programs/ | grep -v 'keycode::'` must return nothing.

Every program that tracks `Modifiers` through `update_modifiers` needs a `KeyRelease`
arm calling it with `pressed = false`, or the first modifier pressed stays set for the
life of the process and an Alt guard disables its shortcuts. Audit by counting
`update_modifiers(.., true)` against `(.., false)` per file.

### Toolkit traps

- `WidgetContainer::get`/`get_mut` return `&dyn Widget` with no downcast, so
  `TextInput::text()` is unreachable once the field is in a container. Keep your own
  copy, updated from `WidgetEvent::TextChanged`; a program that must call `set_text`
  owns the `TextInput` directly.
- `font::Weight` has `Regular`, `Medium` and `Semibold`, no `Bold`.
- A function that borrows a `Surface` for part of a frame restores the clip: use
  `Surface::clipped`, whose `ClipGuard` restores on drop. A leaked clip silently blanks
  everything drawn after it.
- `Surface::blit_region` copies whole rows with `copy_from_slice` and runs on every
  damaged rectangle of every frame. Do not turn it into a per-pixel loop.
- `edos_render::image` has `scaled_to_cover` (wallpaper: fill, crop about the centre)
  and `scaled_to_fit` (viewer: whole picture, never enlarge past 100%) over one
  bilinear `resample_at`.
- `edos-grab` runs one network operation at a time on a worker thread reporting over
  `mpsc`, because two concurrent installs would race over `/var/lib/grab/db`.

### The USB HID driver parses report descriptors

`kernel/src/drivers/usb/hid/report.rs` builds a field map from the report descriptor.
Absolute versus relative is stated by the Input item, and it is the whole difference
between a mouse and a tablet.

- The boot-protocol decoder stays as the fallback, so a parser bug never loses a device
  the fixed layout handles.
- `SET_PROTOCOL` goes only to an interface declaring the boot subclass, and only when
  the fixed layout is what will be decoded. A tablet stalls on it; a parsed mouse sent
  it would switch away from the parsed layout.
- Report length comes from the endpoint descriptor; a tablet reports six bytes.
- `parse_pointer` reads only inside a collection declaring itself a pointer or mouse,
  or a keyboard's vendor X/Y pair would make the keyboard the pointer.

### virtio-gpu

- A response code of `0x0` is not a device answer; real codes are `0x11xx` or
  `0x12xx`. Zero means the response area was read before the device wrote it.
  `VirtioGpu::execute_scratch` waits for the descriptor head its own `push` returned.
  `begin_command()` drains the scratch buffer before every command.
- The hardware cursor is resource 100, created once. `setup_cursor` runs on every
  shape change, so it refills the pixel buffer and re-issues `UPDATE_CURSOR`; a second
  `RESOURCE_CREATE_2D` for 100 answers `0x1203`.
- `virtio-vga,blob=on` needs host `CONFIG_UDMABUF` plus a memfd backend, Linux only.
  Elsewhere `create_resource_blob` fails and the driver silently sets `use_blob =
  false`, so every frame pays a `TRANSFER_TO_HOST_2D` copy; check `use_blob` in the
  driver's init log line.
- MSI-X takes two steps: enabling it on the PCI function, then writing the vector into
  each queue's config, since virtio starts every queue at `VIRTIO_MSI_NO_VECTOR`.
  `set_queue_msix_vector` reads the value back, because a device out of vectors answers
  `NO_VECTOR` instead of failing. `virtio_gpu_irqs` in `/proc/gpu_stats` climbs during a
  window drag when the vector is live.
- The interrupt handler cannot drain the control queue: it sits behind `DISPLAY`, a
  preempt-disabling spinlock the flip holds. The handler publishes a count and wakes;
  the flip path drains.

### The flip sends regions, and the wait for it is separate

`FB_IOCTL_FLIP_RECTS` takes `edos-wm`'s disjoint region list from
`DirtyTracker::coalesced` and issues one `TRANSFER_TO_HOST_2D` per region, then one
`RESOURCE_FLUSH` over their bounding box. That matters under `blob=off`, where each
transfer is a real host copy. `Screen::publish` still copies the bounding box into VRAM
(a guest-local memcpy of correct pixels), and a page-flipping display takes the
bounding box because the other page misses the previous frame's region too.

The wait is `FB_IOCTL_FLIP_WAIT`, called from `Screen::publish` rather than at the
start of the next flip: `publish` writes the buffer the host may still be reading, so
waiting in the flip would tear. It parks at most `FLIP_WAIT_ROUNDS` times
`FLIP_WAIT_SLICE`, then lets the frame through. A display with no interrupt vector
spins in the driver's bounded loop, since nothing would wake a park.

## Userspace programs and the shell

### Program argument traps

A program that treats `args[1]` as "the path" breaks once one shell word expands to
many, and one that indexes `args[1]` without parsing turns every flag into a plausible
success (`mkdir -p` once created `-p`). Use `edos_lib::args`.

There is no `printf` in the guest; build fixtures with `echo` and `>>`.

### `edos_lib` wrappers answer `Result`

- `execve` and `reboot` return only on failure, so they answer `Errno`. A kernel that
  answered success without replacing the image reports `Errno::UNKNOWN`.
- `fork` answers `Result<u64, Errno>` with `Ok(0)` in the child.
- `Ok(0)` is not failure: from `read` it is end of file, from `poll` a timeout. Map `n
  <= 0` to `.unwrap_or(0) == 0`, not `.is_err()`; backwards turns end of input into a
  spin.
- For count-returning calls (`readlink`, `getdents`), `rc != 0` meant "nonzero
  count"; compare against `Ok(n)`. Only `Result<(), Errno>` calls map `!= 0` to
  `.is_err()` mechanically.
- `Errno` has no `Display`; format with `{e:?}`. Its discriminants are the kernel's
  numbers, so `e as i32` feeds `std::io::Error::from_raw_os_error`.
- Build new wrappers on `sys::sys_ok` and `sys::sys_count`
  (`programs/edos_lib/src/sys.rs`).
- `loop { let Ok(n) = f() else { break }; ... }` fails `clippy::while_let_loop`;
  write `while let Ok(n) = f()`.
- To list every call site an API change breaks: `cargo +edos check --all-targets
  --keep-going --message-format=short` in `programs/`. Size a conversion by the lines
  that name the function, not the lines that use its result. A regex rewrite must
  skip std's `read_vectored`/`write_vectored`, which answer plain `usize`.

### The session environment

`SYS_SPAWN` (`edos_lib::process::spawn`) passes no envp at all.
`process::spawn_with_env` goes over `SYS_SPAWN2` with the caller's environment;
`edos-init` and `ChildProcess::spawn_shell` use it. If a session-wide setting does not
reach a program, check which spawn it went through. `HOME`, `PATH` and `PWD` can look
like they work under an empty environment because readers have hardcoded fallbacks.

`TZ` is a fixed ISO 8601 offset signed east (`+02:00`, `-0530`, `+02`, `Z`),
deliberately not POSIX `TZ`. A zone name parses as nothing and means UTC
(`edos_lib::time::utc_offset_seconds`).

### The session has a home directory, and a menu entry needs arguments

`edos-init` sets `HOME` and `USER` and chdirs to `/home/edos` before spawning anything.
The chdir is the load-bearing half: `spawn` copies the parent's cwd. A root with no
`/home/edos` keeps its boot cwd.

`Item::Launch` in `programs/edos-taskbar/src/menu.rs` carries an argument list.
`imgview` and `play` print usage and exit with no file, so their rows pass
`/share/wallpapers/dusk.bmp` and `/share/sounds/chime.wav`. `snake` has no terminal when
the panel spawns it, so its row is `edos-terminal /bin/snake`; `edos-terminal PROG
[ARGS...]` runs PROG on the pty and titles the window after it. `chime.wav` is
generated by `scripts/mksounds.py`, so the repo holds no binaries. Check a program's
behaviour with the launcher's arguments before adding a row.

### Shell redirection, globbing and quoting

- `Redirects` (`programs/edos-sh/src/command.rs`) is an ordered list; order is the
  semantics (`>f 2>&1` versus `2>&1 >f`). `open_redirects` resolves it left to right.
  `&>f` is `>f 2>&1`, never two opens. `split_chain` must not read the `&` in `>&`,
  `<&` or `&>` as the background operator. Only descriptors 0 to 2 can be redirected,
  because `SYS_SPAWN2` takes exactly three.
- Globbing (`programs/edos-sh/src/glob.rs`) expands per path component over `readdir`.
  Components after the last pattern are checked for existence. The quoted flag is per
  word, so `a"b"*` is entirely literal, unlike POSIX.
- Quoting follows POSIX 2.2.2/2.2.3 in `parse_command`: inside single quotes a
  backslash is literal; inside double quotes it escapes only `$`, backtick, `"` and
  `\`. Expansion runs first, over the raw line, so `expand_variables` tracks both
  quote kinds and passes an escaped character through with its backslash for
  `parse_command` to interpret. A here-document body is never parsed, so it goes
  through `expand_heredoc` instead, which treats quotes as literal and consumes the
  backslash before `$`, backtick and `\` (POSIX 2.7.4).
- `prepare_segment` expands a segment and opens its redirections exactly once, because
  expansion runs `$(...)`. A background external job is spawned directly so `fg` can
  hand it the terminal; a background builtin forks and calls `setpgid(0, 0)`. Test job
  control with `cat`, which stops at once.
- `kill` is `kill [-SIGNAL] PID...`, reading the signal only from the `-SIG` position,
  so `kill 27 20` signals pids 27 and 20. Use the name form.
- `edos-sh` prints `\x1b[?25h` before every prompt, because a full-screen program
  killed before restoring the cursor would leave it hidden and there is no `reset`.

### `sed`

`programs/sed/src/regex.rs` is a backtracking engine over `&[char]`, so capture offsets
are character indices. `m_rep` refuses a repetition whose body matched empty (else
`\(a*\)*` never terminates), and `substitute` skips an empty match starting where the
last one ended (else `s/a*/-/g` on `baac` gives `-b--c-`, not `-b-c-`). A sed script
whose output equals its input is more likely a quoting bug: check the argv with
`strace -o /tmp/t.txt sed '...'`.

### `tar` exists, and it reads and writes what GNU tar does

The ustar details `programs/tar/src/main.rs` must keep:

- The checksum is computed with the checksum field as eight spaces and written as six
  octal digits, NUL, space. Other layouts are accepted by some readers and rejected by
  others.
- Numeric fields are `width - 1` zero-padded octal digits plus NUL. The parser accepts
  leading spaces and stops at the first non-digit.
- A path over 100 bytes splits into `prefix` and `name` at a `/`, taking the longest
  prefix that fits.

Check interop both directions against host GNU tar, not only a round trip.

### Terminal output from full-screen and filter programs

- `/proc/processes` publishes only a monotonic `CPUms` per thread, so a CPU share is
  growth over a measured interval. `edos_lib::procinfo::read_table` is the one parser
  and reads every column in order; skipping a field by position is how a reader
  silently falls a column behind when one is added mid-table.
- A line exactly `cols` wide wraps on its own; clip to `cols - 1`. A full-screen frame
  ending in `\r\n` scrolls the screen; write the last row without a line feed.
- Anything that clips, wraps or diffs another program's output parses ANSI escapes
  through `edos_lib::term` (`cells()`, `window()`, `render()`), which also expands
  tabs.
- The terminal widget supports SGR 7/27 through a `reverse` pen flag. End a highlight
  with SGR 27, not SGR 0. DECTCEM `\x1b[?25l`/`h` is `cursor_enabled`, separate from
  the blink phase.
- There is no `/dev/tty`, and devfs `tty0` is the kernel console. In a pipeline stderr
  still points at the PTY, so `less` reads keys from fd 0 when it is a terminal and fd
  2 otherwise. Any interactive program that can end a pipeline does the same.
- `/proc/<tid>/cmdline` comes from `UserThread.cmdline`, captured at load by
  `cmdline_of` because the process may overwrite its stack; `execve` replaces it,
  `clone`/`fork` inherit it. `edos-init` supervises each child from its own thread, so
  `pstree` shows `edos-init-thread-N---edos-wm`.

## Browser (`edos-web`)

`doc/design/browser.md` is the design. What it does not say:

- `edos-web URL` opens a window and prints one summary line; text rendering is behind
  `-d`. A headless assertion on page text is `edos-web -d URL > /dev/klog 2>&1`; a grep
  of `run_log.txt` from a run without `-d` finds only the summary line. `-d -l` lists
  every link target, which is where a dropped `href` shows up.
- An `edos-web` that prints nothing and exits with status 11 died in `html5ever`'s
  parse, not in the walk, since a walk crash emits earlier blocks first.
- Edit the fixture at `assets/welcome.html`, never at
  `filesystem/share/web/welcome.html`. The install is `cp -u`, so an edited installed
  copy is newer than its source, invisible to git, and not restored by `make
  filesystem`.
- An `em` is `doc::ROOT_PX` = `font::size::BODY` = 14 px, not 16, and the window opens
  at `ui::WIN_W` = 760 px, so a `50em` breakpoint is already crossed before a resize
  starts. `welcome.html` writes breakpoints in px.
- A fixture cannot show `font-style`: the theme has no italic face, and `view.rs`
  substitutes `title_accent` only where the run has no colour, while every fixture
  element inherits `body { color }`. Demonstrate selectors with weight, colour or a
  box, and check the cascade with a host test.
- A word-break fixture needs a word longer than the column; measure it against the
  column before reading anything into a fixture that does not break.
- Most of `css::Computed` inherits deliberately (including `shift`), so a new
  non-inherited property must be reset in `Computed::inherit`
  (`programs/edos-web/src/css.rs`), or it applies to every descendant.
- Whether a word is glued to the previous one is carried across runs (`Word::glued` in
  `view.rs`). Getting the carry wrong glues `<b>bold</b> <b>face</b>` while the
  run-together nav case looks unchanged.
- `Computed.decoration` inherits and records no author, so `doc.rs::ua_decoration`
  compares the cascaded value with the parent's to decide whether the UA line for
  `<del>`, `<s>`, `<u>` or `<ins>` applies. Author rules replace the inherited set,
  deliberately unlike CSS, so a page can remove a link's underline inside a decorated
  ancestor. A rule drawn per word fragment comes out dashed, so the draw loop extends it
  to the next fragment with the same decoration.
- `vertical-align`: `Line::natural` in `view.rs` is the tallest face on the line and is
  the baseline; a fragment is drawn at `lead + (natural - own) - shift`. A super- or
  subscript grows the line. `Script::shift` is computed from the block's base px
  before `Script::px` shrinks the run. `vertical-align` inherits here, unlike CSS,
  because the inline model is flat. `top`, `middle`, `bottom`, `text-top` and
  `text-bottom` parse to zero shift.
- `edos_http::Url` wraps the `url` crate for percent-encoding and IDNA. Measured cost
  on a linked release binary: whole `url` +258 KB, of which UTS-46 (`idna`) is 93%;
  against `edos-web` at 5.5 MB it is not worth trimming. `authority()` and the `Host`
  header keep IPv6 brackets; `host()` drops them for the resolver and SNI.
- The stale pooled-connection retry needs a server that closes on demand; `scripts/`
  has none, and Cloudflare holds connections too long. A small Python server that
  keeps a connection N seconds and drops it triggers the path; `run_log.txt` shows
  `Established -> CloseWait` between the two requests.

## Performance measurement

### A ratio's baseline must be measured like its numerator

`balancebench wake` once timed its solo right after writing its header to `/dev/klog`,
without the 60 ms settle every burst got (`WAKE_SETTLE`); the solo grew from 4.16 to
6.80 ms and the ratio read 1.31 instead of 2.00. A ratio flatters itself when its
denominator grows. Derive an expected value from another measurement in the same run:
`WAKE_ROUNDS` is 1/36.4 of `WORK_ROUNDS`, so the default mode's 152.3 ms predicts a
4.19 ms solo, and a 62% disagreement is a harness bug found without a rerun. Both
harness bugs in `balancebench` produced numbers better than reality.

### Driving `switchbench` headless, and bisecting a regression

`scripts/edos-vm start --smp 1`, wait for the `panel|` line in `run_log.txt`, then
`scripts/edos-vm click 400 300` and nothing else. Clicking the taskbar button first
minimises the visible terminal, after which every keystroke lands on the wallpaper.
Then `scripts/edos-vm type 'switchbench 20000 -l' --enter` and wait for `switchbench
sleep worst overshoot` once per run (`-l` mirrors the report to `/dev/klog`).

Bisect a performance regression by checking out whole commits, kernel and userspace
together: an old kernel under current userspace disagrees about the syscall error
convention.

### `poll` costs, and `pollbench` traps

`poll` never reads the clock for a zero timeout and reads it once for a timed wait. One
`PollSet` (`kernel/src/fs/handle.rs`) serves the whole call, so a call allocates 2 or 3
times regardless of descriptor count. Measured marginal cost per descriptor 99 ns,
fixed cost about 162 ns. In a terminal fd 1 is a PTY slave, which takes the PTY lock, so
it is no allocation-free baseline. Single readings of the one-descriptor line swing
between 150 and 256 ns; trust the n >= 2 rows and three-run medians.
