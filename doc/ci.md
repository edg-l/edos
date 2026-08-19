# CI

Workflows live in `.github/workflows/`. Everything runs on GitHub-hosted
`ubuntu-24.04`.

| Workflow | Trigger | What it does |
|---|---|---|
| `ci.yml` | push to `trunk`, pull request | six jobs, below |
| `toolchain.yml` | `toolchain/edos.pin` changes, or by hand | builds the Rust fork and publishes it |
| `iso.yml` | by hand | a bootable ISO as a workflow artifact |
| `dependabot.yml` | dependabot opens a pull request | enables auto-merge on an action bump |

`guest suites` also measures the built image with `scripts/image-sizes` and, on
a pull request from this repository, compares it against the sizes the last
successful trunk run recorded and rewrites one comment with the deltas. A diff
says nothing about what a change costs the live root, which is resident in RAM
for the whole boot. A fork's pull request gets a read-only token, so the
comment step is skipped there rather than failing.

Auto-merge needs **Allow auto-merge** enabled in the repository settings;
without it `gh pr merge --auto` fails and the bump waits for a human.

Releases are not cut by CI. `scripts/release` does that from a local tree; see
`HOW-TO-RELEASE.md`.

## The jobs in `ci.yml`

| Job | Gate |
|---|---|
| `kernel` | `cargo fmt --check`, `make check` (which checks every feature one at a time), `cargo clippy -D warnings` |
| `host tests` | `scripts/host-tests` |
| `host tools` | `make check-mkfs`, `make check-fsck`, and a release build of `efs-mkfs`, `efs-fsck`, `grab-repo` |
| `userspace` | `cargo +edos fmt --all --check`, `make programs`, `cargo +edos clippy --all-targets -D warnings` |
| `in-kernel suite` | `make test AUDIODEV=none`, the `sched-test` suites under KVM |
| `guest suites` | `make guest-check`: iotest, socktest, mmaptest and the rest, in one boot |

**Both trees are clippy-gated at `-D warnings`.** The kernel job has been for a
while; userspace was not, which is how it accumulated 137 warnings nobody saw,
two of them deny-level lints that made `cargo clippy` on `programs/` fail
outright and so hid the rest. `make clippy` runs both locally, and `make
fmt-check` is the reporting form of `make fmt`. Run them before pushing:
they are cheap, and they are the two gates most likely to red CI on a change
that otherwise works.

`clippy::too_many_arguments` is allowed globally -- `kernel/src/main.rs` for the
kernel, `programs/.cargo/config.toml` for the 131-member workspace. Splitting a
driver's submit path or a widget's draw call into argument structs to satisfy a
count is churn that makes the call sites harder to read; where a bundle
genuinely clarifies, it is worth doing on its own merits. Everything else is
meant to stay at zero.

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

Five gates never run in CI: `nvme-check`, `storage-check`, `recovery-check`,
`orphan-check` and `ssh-check`. They are multi-boot harnesses: they cut power
on a disk mid-write, reboot between a write phase and a verify phase, boot a
purpose-built ISO, or drive the host's own OpenSSH client at the guest. The
table in `doc/WORKING-NOTES.md` says what each one costs and needs. The
consequence is worth stating plainly: **a regression in the NVMe driver, in the journal's replay
path, in orphan reclamation or in `sshd` is visible only in a local run.**
`ci.yml` boots a guest in exactly two jobs, for the in-kernel suite and the 17
guest suites, and nothing else boots.

`scripts/release prepare` does not close the gap either. It gates on the kernel
check, clippy, both formatters, the host tests, a userspace build and `make
test`; it runs neither `guest-check` nor any of the five.

Adding them is a judgement about runner minutes rather than a technical
obstacle: they need `/dev/kvm`, which the `Enable KVM` step already arranges,
and they take no arguments. The shape that fits is one scheduled workflow
running the five against `trunk`, not five more jobs in front of every pull
request. `nvme-check` alone builds two ISOs and two disk images and boots four
guests: 2 min 25 s on this host with every build already warm, and a runner
pays the cold build on top, which is a `guest-check` job's work over again, and the driver it gates moves far less often than the tree
around it. A nightly catching an NVMe regression that a pull request introduced
and `trunk` shipped is the evidence for promoting `nvme-check` to the
pull-request path; until that happens, running it on every push pays for a
class of failure nobody has seen.
