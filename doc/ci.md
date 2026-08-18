# CI

Workflows live in `.github/workflows/`. Everything runs on GitHub-hosted
`ubuntu-24.04`.

| Workflow | Trigger | What it does |
|---|---|---|
| `ci.yml` | push to `trunk`, pull request | six jobs, below |
| `toolchain.yml` | `toolchain/edos.pin` changes, or by hand | builds the Rust fork and publishes it |
| `iso.yml` | by hand | a bootable ISO as a workflow artifact |

Releases are not cut by CI. `scripts/release` does that from a local tree; see
`HOW-TO-RELEASE.md`.

## The jobs in `ci.yml`

| Job | Gate |
|---|---|
| `kernel` | `cargo fmt --check`, `make check` (which checks every feature one at a time), `cargo clippy -D warnings` |
| `host tests` | `scripts/host-tests` |
| `host tools` | `make check-fsck`, and a release build of `efs-mkfs`, `efs-fsck`, `grab-repo` |
| `userspace` | `cargo +edos fmt --check`, `make programs` |
| `in-kernel suite` | `make test AUDIODEV=none`, the `sched-test` suites under KVM |
| `guest suites` | `make guest-check`: iotest, socktest, mmaptest and the rest, in one boot |

The two booting jobs need `/dev/kvm`. It exists on the hosted runners but is
not writable by the runner user until a udev rule says so, which is the `Enable
KVM` step; without it QEMU falls back to TCG and both jobs time out rather than
failing with anything readable. Both upload `run_log.txt` on failure, which is
the first thing to read when one of them goes red.

## The `edos` toolchain

Userspace links a real `std` from the fork at
[edg-l/rust](https://github.com/edg-l/rust/tree/edos_std_v3), so every job that
touches `programs/` needs a compiler that no rustup channel provides. Building
it takes hours, which is too long to put in front of a pull request, so it is
built once and installed everywhere else:

- `toolchain/edos.pin` names the fork revision the tree builds against. It is
  the only place that is recorded.
- `toolchain.yml` builds that revision with `./x install` and uploads the
  install prefix as `edos-toolchain-<rev>.tar.zst`, an asset on the
  `edos-toolchain` release. That release is a prerelease so the ISO stays the
  latest thing on the releases page, and it accumulates one asset per revision,
  so an older pin still resolves.
- `.github/actions/edos-toolchain` installs it: `actions/cache` keyed on the
  revision, falling back to downloading the asset, then `rustup toolchain link
  edos`. A revision with no asset is an error naming the workflow to run, not a
  two-hour build.

`./x install` runs with `--ci=false`. Bootstrap autodetects CI from
`GITHUB_ACTIONS` and then assumes it is rust-lang's own CI, where the checkout
is two commits deep and HEAD is a merge commit, so `HEAD^1` names an upstream
bors commit that has a prebuilt LLVM. A fork's branch is linear, so `HEAD^1` is
merely the previous fork commit and `ci-artifacts.rust-lang.org` answers 404.
Told it is not CI, bootstrap searches back for the last upstream commit that
touched `src/llvm-project`, `src/bootstrap/download-ci-llvm-stamp` or
`src/version`, which is the commit that actually has an LLVM to download.

So moving the fork forward is two steps, in this order:

1. Push the fork, then edit `EDOS_RUST_REV` in `toolchain/edos.pin` and push
   that. The push builds and publishes the new toolchain.
2. Once that workflow is green, everything else picks it up.

Doing it the other way round leaves every userspace job failing with `no
prebuilt toolchain for <rev>` until the build lands. To build a revision
without moving the pin, run `toolchain.yml` by hand with a `rev` input.

The same asset is the fastest way for a contributor to get a working
toolchain without building the fork:

```bash
gh release download edos-toolchain --repo edg-l/edos --pattern 'edos-toolchain-*.tar.zst'
mkdir -p ~/edos-toolchain && tar --zstd -xf edos-toolchain-*.tar.zst -C ~/edos-toolchain
rustup toolchain link edos ~/edos-toolchain
```

## What is not covered

`storage-check`, `recovery-check`, `orphan-check` and `ssh-check` are multi-boot
harnesses that cut power on a disk or drive the host's SSH client at the guest.
They run locally, and `scripts/release prepare` does not gate on them either.
