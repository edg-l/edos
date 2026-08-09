# Rebasing `edos_std_v2` onto upstream Rust

Survey of what it takes to move the fork at `~/dev/rust` (branch `edos_std_v2`,
remote `origin` = github.com/edg-l/rust) onto current rust-lang/rust. Measured
2026-08-09 against upstream head `153ecc4f740`; re-measure before acting on it.

## Divergence

| | |
|---|---|
| Merge-base | `0312a55fe4200208170b94bf287ca3cc7ea499ae` (2026-04-04) |
| Fork ahead | 7 commits |
| Fork behind | 13,243 commits |
| Version gap | `src/version` 1.96.0 → 1.99.0 |

Two facts that invalidate older notes:

- **Upstream's default branch is `main`, not `master`.** Anything written as
  `origin/master` has to become `upstream/main`.
- The clone has **no upstream remote**, and `origin`'s refspec is narrowed to
  `+refs/heads/edos_std_v2:refs/remotes/origin/edos_std_v2`, so there is no
  upstream-tracking ref to diff against until one is added.
- `~/dev/rust/bootstrap.toml` is gitignored, so the local build config survives
  any rebase untouched.

The 7 commits:

```
d93cfe2040a  add edos OS target support (x86_64-unknown-edos)  63 files, +2067 -16
c1f7b924405  Add AsRawFd, OwnedFd, and IsTerminal support      18 files,  +299 -75
b7af81795f6  std: edos OpenOptions must carry the access mode   1 file,   +39  -6
34a7eb4bcb5  std: move edos to edos_rt 0.0.36                   2 files,   +3  -3
f6893b07414  std: edos keeps the kernel's error code            8 files,  +49 -54
61477130d79  std: implement net for edos                        4 files, +583  -4
fbb05df2c53  std: fix SystemTime::now and DirEntry::path        5 files,  +19 -28
```

## What the fork touches

73 files: **27 new EDOS-only files**, **37 real modifications**, **9 submodule
pointer changes that are pure noise**.

### New files (cannot conflict)

- `compiler/rustc_target/src/spec/base/edos.rs`,
  `compiler/rustc_target/src/spec/targets/x86_64_unknown_edos.rs`
- `library/std/src/os/edos/{mod,ffi/mod,ffi/os_str,io/mod}.rs`
- `library/std/src/sys/…`: `alloc/edos.rs`, `env/edos.rs`, `fd/edos.rs`,
  `fs/edos.rs`, `io/error/edos.rs`, `io/is_terminal/edos.rs`,
  `net/connection/edos.rs`, `paths/edos.rs`, `pipe/edos.rs`, `process/edos.rs`,
  `random/edos.rs`, `stdio/edos.rs`, `thread/edos.rs`, `time/edos.rs`,
  `pal/edos/{common,futex,mod,os,pipe,start,time}.rs`

### Noise to drop, not carry

`d93cfe2040a` also moved 9 submodule pointers **backwards**
(`library/backtrace`, `src/llvm-project`, `src/tools/cargo`,
`src/doc/{book,edition-guide,embedded-book,nomicon,reference,rust-by-example}`)
plus `src/tools/rustbook/Cargo.lock`. Local `./x` drift, nothing to do with
EDOS. Carrying an April LLVM onto an August stage0 breaks the build.

### The load-bearing modifications

- `compiler/rustc_target/src/spec/mod.rs` — a `supported_targets!` entry and
  `Os::Edos = "edos"`.
- `library/std/Cargo.toml` — `edos_rt = { version = "0.0.41", features =
  ['rustc-dep-of-std'], public = true }` under `cfg(target_os = "edos")`.
- `library/std/src/os/fd/raw.rs` — `RawFd = i32`, literal `STD*_FILENO`.
- `library/std/src/os/fd/owned.rs` — **the most invasive hunk**: restructures
  upstream's `Drop for OwnedFd` body so an edos arm can sit outside the shared
  `unsafe {}`, adds `try_clone_to_owned`, and opens eight `not(trusty)` gates on
  the net ↔ `OwnedFd` impls.
- `library/std/src/sys/paths/mod.rs`, `sys/thread/mod.rs` — inline `mod imp`
  cherry-picking a few edos symbols and falling back to `unsupported`.
- `src/bootstrap/src/lib.rs` — `EXTRA_CHECK_CFGS` entry, and
  `compiler-builtins-mem` for edos.
- `src/bootstrap/src/core/sanity.rs` — `STAGE0_MISSING_TARGETS`.

The remaining 28 are one-line `target_os = "edos"` additions to cfg lists.

## Conflict risk, measured

Reconstructing the three-way merge for all 37 modified files (base = merge-base
blob, ours = fork blob, theirs = current `main`) gives **28 clean merges and 9
conflicts, each a single hunk**.

**The `sys/` reorganization did not happen in this window.** `library/std/src/sys/`
has byte-identical directory listings at the merge-base and at `main`, and so
does `sys/pal/`. No file the fork touches has moved or disappeared.

| File | Cause | Resolution |
|---|---|---|
| `sys/sync/{condvar,mutex,once,thread_parking}/mod.rs` | upstream added a wasi-p3 arm to the same futex list | keep both lines |
| `sys/sync/rwlock/mod.rs` | same, plus a re-indent both sides made | take upstream's `motor` line |
| `src/bootstrap/src/core/sanity.rs` | `STAGE0_MISSING_TARGETS` was emptied | keep upstream's entry, add ours |
| `src/tools/rustbook/Cargo.lock` | noise vs noise | drop the fork side |
| `library/std/src/os/fd/owned.rs` | upstream edited the POSIX-close comment inside the restructured block | reapply by hand |
| `library/std/src/os/mod.rs` | upstream converted the per-platform ladder to `cfg_select!` (198 → 147 lines) | both insertion points survive; two lines |

## The real work: semantic breakage

None of this shows up as a conflict. It only appears when you build.

1. **The futex module moved.** `library/std/src/sys/sync/futex/` is new on main
   and every `sys/sync/*` consumer imports from `crate::sys::sync::futex`. Move
   `sys/pal/edos/futex.rs` → `sys/sync/futex/edos.rs`, drop `pub mod futex;`
   from `pal/edos/mod.rs`, and add a `target_os = "edos"` arm to
   `sys/sync/futex/mod.rs`. The required surface is unchanged, so it is a move
   plus a registration, not a rewrite.
2. **`BorrowedCursor<'_>` gained a generic parameter** → `BorrowedCursor<'_, u8>`.
   9 sites: `sys/fd/edos.rs` (2), `sys/fs/edos.rs` (2),
   `sys/net/connection/edos.rs` (2), `sys/pal/edos/pipe.rs` (2),
   `sys/stdio/edos.rs` (1).
3. **`advance_unchecked` → `advance`.** 4 sites: `sys/fd/edos.rs`,
   `sys/net/connection/edos.rs` (2), `sys/stdio/edos.rs`.
4. **`sys/fs` requires `imp::set_perm_nofollow`.** Every backend must provide
   it now; model on `fs/unsupported.rs`.
5. **`sys/process` requires `Command::get_resolved_envs`** returning
   `CommandResolvedEnvs`.
6. **`TcpStream` requires `set_keepalive` / `keepalive`.**
7. `lookup_host_string` is new but the fork's `lookup_host` signature already
   matches, so it is free.
8. `sys/io/mod.rs` dropped the `io_slice` module. The fork already imports
   `IoSlice` from `crate::io`, so it is unaffected.

Verified non-issues: `cfg_select!` is unchanged; target registration is
unchanged and all 19 `TargetOptions` fields the fork sets still exist; both
bootstrap anchors survive; `sys/pal/unsupported/` is byte-identical.

## Two pre-existing defects to fix in the same pass

- `tests/assembly-llvm/targets/targets-elf.rs` registers the revision as
  `--target x86_64-unknown-linux-edos`. That triple does not exist; the fork
  defines `x86_64-unknown-edos`.
- `sys/pal/edos/` still carries `pipe.rs`, `time.rs` and `os.rs` while
  upstream's `pal/unsupported/` is down to `common.rs` + `mod.rs`. Hoisting
  them alongside the mandatory futex move would leave `pal/edos` as just
  `common.rs` + `start.rs` + `mod.rs` and shrink the next rebase.

## Recommendation: rebase, not merge

- 27 of 73 files cannot conflict; 28 of the 37 modified files auto-merge; the 9
  that conflict do so with one hunk each, six of them "keep both lines".
- Conflicts are confined to the two oldest commits. The five most recent —
  including the 583-line net implementation — touch only EDOS-owned files and
  replay untouched.
- A merge would fold the stale submodule pointers back in as regressions.
- Rebase difficulty scales with the fork's surface, not with upstream's commit
  count, and the surface is small.

## Plan

```bash
cd ~/dev/rust
git remote add upstream https://github.com/rust-lang/rust.git
git fetch upstream main
git branch edos_std_v2_backup edos_std_v2
git switch -c edos_std_v3 edos_std_v2

# Strip the noise on the old base first, so it never becomes a conflict.
git checkout 0312a55fe42 -- \
  library/backtrace src/llvm-project src/tools/cargo \
  src/doc/book src/doc/edition-guide src/doc/embedded-book \
  src/doc/nomicon src/doc/reference src/doc/rust-by-example \
  src/tools/rustbook/Cargo.lock
git commit -m "revert incidental submodule and rustbook lockfile drift"

git rebase --onto upstream/main 0312a55fe42 edos_std_v3
```

Then the semantic fixes above, then:

```bash
./x check library --target x86_64-unknown-edos   # surfaces every item in one pass
./x install
cd ~/dev/edos-v2/programs && cargo +edos clean
cd ~/dev/edos-v2 && make programs
```

`cargo +edos clean` is not optional: artifacts from a 1.96 sysroot against a
1.99 one fail with metadata mismatches that read like unrelated errors.

Finally boot it, because a compiling std proves nothing about `edos_rt` ABI
drift across three release cycles. Exercise what the recent commits touched:
net, `SystemTime::now`, `DirEntry::path`, and process spawn/waitpid via
`edos-init`.

If `edos_rt` itself needs a change, the loop is patch → bump version →
`cargo publish` → bump the exact `edos_rt = "0.0.z"` in the fork's
`library/std/Cargo.toml` → `cargo +nightly update --manifest-path
library/Cargo.toml -p edos_rt` → `./x install` → `cargo +edos clean` →
`make programs`. A `0.0.z` requirement is exact, so a skipped pin bump silently
ships the old crate.
