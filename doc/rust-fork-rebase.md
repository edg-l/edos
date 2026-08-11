# Rebasing the std fork onto upstream Rust

The fork at `~/dev/rust` (remote `origin` = github.com/edg-l/rust) carries EDOS
target support on top of rust-lang/rust. It was rebased from 1.96.0 to **1.99.0**
on 2026-08-11, onto upstream `0e72e3266cd`, skipping 13,491 upstream commits.

## The shape of the fork

63 files, +2853 −44: **27 EDOS-only files that cannot conflict**, and 36 real
modifications to shared ones. Rebase difficulty scales with that surface, not
with how far upstream has moved, which is why a three-month gap cost one
afternoon.

The EDOS-only files:

- `compiler/rustc_target/src/spec/base/edos.rs`,
  `compiler/rustc_target/src/spec/targets/x86_64_unknown_edos.rs`
- `library/std/src/os/edos/{mod,ffi,io/mod}.rs`
- `library/std/src/sys/…`: `alloc/edos.rs`, `env/edos.rs`, `fd/edos.rs`,
  `fs/edos.rs`, `io/error/edos.rs`, `io/is_terminal/edos.rs`,
  `net/connection/edos.rs`, `paths/edos.rs`, `pipe/edos.rs`, `process/edos.rs`,
  `random/edos.rs`, `stdio/edos.rs`, `sync/futex/edos.rs`, `thread/edos.rs`,
  `time/edos.rs`, `pal/edos/{common,mod,start}.rs`

The load-bearing modifications:

- `compiler/rustc_target/src/spec/mod.rs` — a `supported_targets!` entry and
  `Os::Edos = "edos"`.
- `library/std/Cargo.toml` — the `edos_rt` pin under `cfg(target_os = "edos")`.
- `library/std/src/os/fd/raw.rs` — `RawFd = i32`, literal `STD*_FILENO`.
- `library/std/src/os/fd/owned.rs` — the most invasive hunk: restructures
  upstream's `Drop for OwnedFd` so the edos arm, a safe call, sits outside the
  shared `unsafe {}`.
- `library/std/src/sys/paths/mod.rs`, `sys/thread/mod.rs` — inline `mod imp`
  cherry-picking a few edos symbols and falling back to `unsupported`.
- `src/bootstrap/src/lib.rs` — `EXTRA_CHECK_CFGS`, and `compiler-builtins-mem`.
- `src/bootstrap/src/core/sanity.rs` — `STAGE0_MISSING_TARGETS`.

The rest are one-line `target_os = "edos"` additions to cfg lists.

## What the 1.96 → 1.99 rebase actually cost

**Conflicts were cheap.** Nine files, one hunk each. Six were "keep both lines"
where upstream had added a `wasi-p3` or `motor` arm to a cfg list the fork also
extends. `sanity.rs` needed upstream's entry plus ours. `os/fd/owned.rs` had to
be reapplied by hand. `os/mod.rs` had moved `pub mod fd;` above the per-OS
ladder, so the fork's copy was deleted and the `edos` arm added to upstream's.

**Semantic breakage was the real work**, and none of it shows up as a conflict:

1. Futex implementations moved to `sys/sync/futex/`. `pal/edos/futex.rs` moved
   to `sys/sync/futex/edos.rs` and registered an arm there.
2. `BorrowedCursor<'_>` gained an element type: `BorrowedCursor<'_, u8>`.
3. `advance_unchecked` became `advance`.
4. `sys/fs` requires `set_perm_nofollow`, `sys/process` requires
   `Command::get_resolved_envs`, `TcpStream` requires `set_keepalive`/`keepalive`.
5. `sys/alloc` backends are free functions now, not a `GlobalAlloc for System`
   impl. `alloc_zeroed` and `realloc` come from fallbacks std synthesizes.
6. `crate::sealed::Sealed` is gone, replaced by `pub impl(self) trait`.
7. `implicit_provenance_casts` is denied in std: a pointer handed to the kernel
   as an integer needs `expose_provenance()`, since the kernel dereferences it.

Only 2, 3 and 7 were mechanical. The rest each needed a decision about what the
kernel can actually do.

## Traps

- **The submodule noise cannot be stripped by a commit on top.** The fork's
  first commit moved nine submodule pointers backwards; a revert commit lands
  *after* it in replay order, so it never pre-empts the conflict. Resolve the
  submodules to upstream's pointers during that commit's replay instead
  (`git checkout upstream/main -- <paths>`) and drop the revert.
- **`cargo +edos clean` is not optional.** Artifacts from an older sysroot fail
  against a new one with metadata mismatches that read like unrelated errors.
- **A compiling std proves nothing about `edos_rt` ABI drift.** Boot it and
  exercise what the recent commits touched.
- The clone's `origin` refspec is narrowed to the EDOS branch, so there is no
  upstream-tracking ref until `git remote add upstream` is run.
- `~/dev/rust/bootstrap.toml` is gitignored, so the local build config survives
  a rebase untouched.

## Doing it again

```bash
cd ~/dev/rust
git fetch upstream main
git branch <backup> <current>
git switch -c <working> <current>
git rebase --onto upstream/main $(git merge-base <current> upstream/main) <working>
```

Resolve as above, then:

```bash
./x check library --target x86_64-unknown-edos   # surfaces every item in one pass
./x check library                                # the fork edits shared files too
./x fmt
./x install
cd ~/dev/edos-v2/programs && cargo +edos clean
cd ~/dev/edos-v2 && make all && make sata-disk.img
```

`make sata-disk.img` matters: every `run` target attaches that image and the
kernel prefers it over the live root, so without it the guest boots the previous
binaries and any verification is meaningless.

Rebase rather than merge. A merge folds the stale submodule pointers back in as
regressions, and the conflicts are confined to the two oldest commits: everything
since touches only EDOS-owned files and replays untouched.

## The `edos_rt` loop

A change to the runtime crate only reaches userspace once it is published and the
fork's pin moves: patch `edos_rt`, bump the version, `cargo publish`, bump the
exact `edos_rt = "0.0.z"` in `library/std/Cargo.toml`, `cargo +nightly update
--manifest-path library/Cargo.toml -p edos_rt`, `./x install`, `cargo +edos
clean`, `make programs`. A `0.0.z` requirement is exact, so a skipped pin bump
silently ships the old crate.
