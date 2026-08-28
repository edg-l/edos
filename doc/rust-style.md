# Rust style

The conventions this tree is written to, where each came from, and what is
deliberately not adopted. `CLAUDE.md` carries the rules that change how code is
written; this file carries the reasoning and the measurements behind them, so a
rule can be re-derived rather than taken on faith.

## Where the rules come from

The reference is the Linux kernel's own Rust coding guidelines
(`Documentation/rust/coding-guidelines.rst`, rendered at
<https://docs.kernel.org/rust/coding-guidelines.html>). It is the only published
rule set written for this situation: a `no_std` kernel with no public API
surface, a C ABI on both sides, and `unsafe` concentrated in drivers. Where it
speaks, it wins.

Two secondary sources, both narrow:

- The standard library's own policy on unsafe documentation
  (<https://std-dev-guide.rust-lang.org/policy/safety-comments.html>). It states
  the same rule as the kernel's, and this tree forks std, so following it keeps
  `edos_rt` and the fork's `library/std` written in one voice.
- ANSSI's Secure Rust Guidelines (<https://anssi-fr.github.io/rust-guide/>) for
  the chapters that match the invariants this tree already has: `Drop`,
  `Send`/`Sync`, raw pointers, uninitialized memory, integer operations. Useful
  as a name for discipline already followed rather than as new work, and dated
  2020 in places.

Guidance written for library and service crates does not transfer and is not
imported: application error crates, a global allocator swap, `target-cpu`,
structured logging over OpenTelemetry, async stack sizing, FFI/DLL state
isolation, MSRV policy, proc-macro crate splits, and the whole of the Rust API
Guidelines' public-surface material. This tree publishes one crate, `edos_rt`,
and it is the exception that those documents describe.

The Safety-Critical Rust Consortium's coding guidelines
(<https://coding-guidelines.arewesafetycriticalyet.org/>) are the right shape and
are not yet written: at version 0.1 the Unsafety, Concurrency and Exceptions
chapters render zero guidelines. Re-check before assuming that is still true.

## Unsafe is documented twice, for two different readers

These are separate jobs and satisfying one does not satisfy the other. Both are
kernel-guideline rules and both are std policy.

**`// SAFETY:` on the block** says why *this* operation upholds a contract
someone else stated. It goes immediately above the `unsafe {`.

**`# Safety` in the doc comment** says what the contract *is*, for callers of an
`unsafe fn` or implementors of an `unsafe trait`. It goes on the item, after the
summary sentence.

An `unsafe fn` whose body performs unsafe operations needs both: the section for
its callers, and a block comment for each operation inside it. Edition 2024
makes `unsafe_op_in_unsafe_fn` warn by default, and CI denies warnings, so the
compiler already forces the inner blocks to exist; nothing yet forces either
comment.

Coverage in `kernel/src`, measured 2026-08-28:

| quantity | count | command |
| --- | --- | --- |
| `unsafe { ... }` blocks | 729 | `grep -rhoE 'unsafe \{' kernel/src --include='*.rs' \| wc -l` |
| `// SAFETY:` comments | 770 | `grep -rhoE '//\s*SAFETY:' kernel/src --include='*.rs' \| wc -l` |
| `unsafe fn` declarations | 65 | `grep -rhoE '\bunsafe fn ' kernel/src --include='*.rs' \| wc -l` |
| `# Safety` sections | 75 | `grep -rhoE '^\s*(///\|//!) # Safety' kernel/src --include='*.rs' \| wc -l` |
| `unsafe impl` | 41 | `grep -rhoE 'unsafe impl' kernel/src --include='*.rs' \| wc -l` |

So the block half is done: `undocumented_unsafe_blocks` is denied crate-wide in
`kernel/Cargo.toml` and reports nothing, which is why there are more `// SAFETY:`
comments than blocks -- a comment also sits above each `unsafe impl` and above
several `unsafe fn` bodies whose single block is the whole function. The
contract half has more `# Safety` sections than `unsafe fn` declarations,
because a section also belongs on an `unsafe trait` and on a safe function whose
contract lives in a raw pointer argument; what it means is that every `unsafe
fn` in a module I5a has been through carries one. The
two are `ROADMAP-CLEANUP.md` I5, and they are different work: the lint
`clippy::undocumented_unsafe_blocks` finds blocks and `unsafe impl`, and finds
nothing about an undocumented `unsafe fn`, whose contract has no lintable form.

`unsafe` marks a risk of undefined behaviour and nothing else. A function that is
merely dangerous to call is a safe function with a `# Panics` section or a
`Result`, not an `unsafe fn`. `programs/fstest`'s `edos_sync` was the tree's last
example of the mistake: a `sync` syscall taking no arguments, so its caller had
nothing to uphold, and it is now a safe fn over an `unsafe` block.

The converse rule does not hold, and the plausible version of it is wrong.
Sixteen `unsafe fn` in this tree have a body with no `unsafe` operation in it --
`BlockBuffer::owned` stores a raw pointer, `BitmapFrameAllocator::deallocate_frame`
returns a frame to the bitmap, `uaccess::setup_fault_resume` hands out the
address of per-CPU state. Each is safe to *execute* and unsound to *have
executed*: the undefined behaviour is deferred to whoever next dereferences the
pointer, maps the frame, or migrates off the CPU. The contract is the reason for
`unsafe fn`, not the presence of a block, so "no `unsafe` block, therefore it
should be a safe fn" would strip the marker off exactly the functions whose
obligation outlives the call.

No lint finds a missing `# Safety` section and none can, so the sweep is a
script: walk back from each declaration over its attributes and doc comment and
look for the heading. A plain `grep` is not enough in either direction -- it
reports the seven declarations preceded by a multi-line `#[expect(...)]` as
missing when they are not, and `\bunsafe fn ` misses `unsafe extern "C" fn`
entirely, which is where the kernel's three entry points live.

## Traits

There are seven traits in `kernel/src` and three across `programs/`, so this tree
has no trait-design problem and no rules are invented for one. `FileSystem`,
`AsyncBlockDevice`, `DevFsDevice`, `NetDevice`, `PageCacheOps`, `Pollable` and
`CancellableOp` all exist for runtime polymorphism over a set decided by what is
mounted or enumerated, which is why they are reached through `dyn` at 68 sites
rather than as generic bounds. Library guidance that ranks generics above `dyn`
is answering a different question; a generic parameter cannot express "whichever
filesystem this mount turned out to be". Do not convert them.

The obligation that does bind here comes from the other direction, from traits
this tree implements rather than declares. `kernel/src` declares one `unsafe
trait` -- `virtio::gpu::DeviceResponse`, the bound that lets a device response be
read out of a DMA buffer by a safe function -- and writes 41 `unsafe impl`: 18
`Send`, 14 `Sync`, the three `DeviceResponse`, plus `GlobalAlloc` and
`FrameAllocator`. All of them carry a `// SAFETY:` comment.

A bare `unsafe impl Send for T {}` is a claim that no data race can arise from
moving `T` between threads, made by hand, in a preemptive SMP kernel with
work-stealing, where the compiler has stopped checking. It is the one shape both
ANSSI and the library guidance name as the canonical unsound shortcut, and the
argument behind each is exactly the kind that decays silently when the type
later grows a field. Every `unsafe impl` states its argument in a `// SAFETY:`
comment above it, naming what provides the exclusion:
`allocator.rs:130` is the model ("only accessed by its owning CPU with IRQs
off"). `clippy::undocumented_unsafe_blocks` covers impls as well as blocks, so
this half is gated by the same lint.

## `#[expect]`, not `#[allow]`

An `#[expect]` warns when the lint it names stops firing, so a suppression
cannot outlive the code that needed it; an `#[allow]` accumulates silently. The
tree is mostly there already: 95 `#[expect(` against 35 `#[allow(` in
`kernel/src`, and 17 `#[allow(` across `programs/`.

The kernel guidelines name the three cases where `allow` is still correct, and
all three occur here:

- conditional compilation, where the lint fires in one feature set and not
  another. `#[cfg_attr(not(feature = "x"), allow(dead_code))]` is the form.
- inside a macro, where whether the lint fires depends on the expansion.
- architecture-specific warnings.

Every suppression states why the item stays. Prefer the `reason = "..."` field
over a comment above the attribute, because a lint can require the field and
cannot require a comment.

## Panics

A function that can panic says so in a `# Panics` section naming the condition.
There is one such section in `kernel/src` today against roughly 84 `unwrap()`
and `expect()` sites, so this is close to unstarted.

An intentional panic carries a message with the values in it. In this tree the
message reaches `run_log.txt`, which is the artifact a boot failure is diagnosed
from, so `expect("...")` naming the violated invariant is strictly better than a
comment above it saying the same thing. `assert!(x >= N, "... got {x}, need {N}")`
over a bare `assert!`.

The standing lesson about `unwrap` is unchanged and is upstream of all of this:
an `unwrap` usually asserts a *phase* that need not exist. Ask what it asserts
before converting it. AHCI and xHCI were two-phase constructors and the fix was
`Arc::new_cyclic`; ACPI's was a `Once::get().unwrap()` pair and the fix was making
each accessor its own `call_once`.

## Naming and comments

- Do not repeat the namespace in an item name: `gpt::Partition`, not
  `gpt::GptPartition`. Callers disambiguate with the path.
- A type wrapping a hardware or spec concept takes a name as close to the spec's
  as Rust casing allows, so the register table and the code can be read side by
  side.
- Comments are Markdown, capitalized, and end with a period. Tagged comments
  (`// SAFETY:`, and nothing else in this tree, since `TODO`/`FIXME`/`HACK`/`XXX`
  are all zero) follow the same rule.
- A doc comment's first paragraph is one sentence describing the item.
- `rustfmt` with default settings decides formatting. There is no local style.

What comments must not contain is in `CLAUDE.md` and is not repeated here.

## The lint set

There is no `[lints]` table in any manifest in the tree; the kernel and the
programs workspace both run clippy at its default level under `-D warnings`.
That is correctness, style, complexity, perf and suspicious, and it is genuinely
clean across the kernel's default, `sched-test`, `trace` and `sched-prof` builds.

What is worth adding, and only this:

```toml
[lints.clippy]
undocumented_unsafe_blocks = "deny"   # I5a, the block half -- done
unnecessary_safety_comment = "warn"   # a SAFETY: on something that is not unsafe
unnecessary_safety_doc = "warn"       # a # Safety section on a safe item
allow_attributes_without_reason = "warn"
```

The two `unnecessary_*` lints are the reason to adopt these as a set rather than
`undocumented_unsafe_blocks` alone: without them the cheapest way to satisfy the
first is to write a comment that says nothing, and the coverage number then
measures comments instead of arguments.

`undocumented_unsafe_blocks` was enabled a module at a time rather than tree-wide
at once, each finished module carrying `#[deny(...)]` on its own `mod`
declaration so it could not regress while the rest caught up. Turning it on
whole would have produced hundreds of findings at once -- `usb/xhci/mod.rs` held
77 blocks, `ahci/port.rs` 39, `thread/scheduler.rs` 33 -- and been abandoned.
With the last module done the ratchets came out and the lint moved into the
manifest, which is the shape to repeat for any lint this tree adopts against an
existing body of code.

Rejected, with the reason, so it is not re-proposed:

- **`clippy::pedantic` and the `restriction` group wholesale.** On 116k lines
  this is a multi-day project with a poor ratio of findings to churn, and it
  collides head-on with B2 and B3, which are about to rewrite the eight files
  with the highest lint density.
- **A structured-logging framework.** `log!` is `format_args!` with no
  allocation and `log_debug!` checks `debug_logging()` before formatting, so the
  overhead rule is already satisfied. Nothing consumes `/dev/klog` structurally,
  and named events plus semantic conventions buy nothing a ring buffer read by
  `dmesg` can use.
- **A sweep for `with_capacity`, `shrink_to_fit`, boxed slices or a faster
  hasher.** Unmeasured performance edits are what `STORAGE-ROADMAP.md`'s refuted
  list is made of; two entries there sounded obviously right and made the system
  slower. Reach for `profile` and `fsbench` first, then change one thing.
- **`[workspace.dependencies]` for the programs workspace.** Only `flate2`,
  `sha2` and `ed25519-dalek` have more than one consumer, and the 85
  `edos_lib = { path = "../edos_lib" }` lines carry no version to drift. The
  134 duplicated `version` and `edition` keys could inherit from the workspace
  and that is the whole of what it would buy.
