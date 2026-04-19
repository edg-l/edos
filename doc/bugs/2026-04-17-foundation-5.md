# Foundation #5: per-inode async I/O registry

**Date**: 2026-04-17
**Status**: Shipped on trunk (phases 0-4, validated in phase 5).
**Plan**: `.claude/plans/foundation-5-inflight-registry.md`
**Audit**: `doc/bugs/2026-04-17-inflight-audit.md`

## Motivation

Prior to Foundation #5 the page cache used a single `InodePages::get_or_fill`
method that held `pages(40)` across the fill backend call. Two readers of the
same uncached page could:

- Both miss under `pages(40)`.
- Race to install a new `Arc<CachedPage>` once the backend returned; the loser
  threw its just-filled page away.
- Both issue redundant AHCI reads for the same block range.

The synchronous dedup worked but left no seam for an async backend. Async NCQ
readahead (plan at `.claude/plans/async-ncq-readahead.md`) needs a first-class
"fill is in progress" state that waiters can park on before the AHCI command
has completed. Foundation #5 introduces that state as an explicit object:
`PageFillHandle`, stored in `InodePages::in_flight` at lock rank 42.

## Design

### `PageFillHandle`

`kernel/src/fs/page_fill.rs`. One handle per in-flight fill, shared across every
waiter via `Arc`. Fields:

- `inode: Weak<VfsInode>` — for the `CancellableOp::cancel` cleanup path; we do
  not keep the inode alive past reader interest.
- `page_idx: u64`, `len: u64` — the range `[page_idx, page_idx + len)` this
  fill covers. Single-page fills have `len = 1`; bulk fills install the same
  `Arc` at every key in the range.
- `waiters: WaitQueue` — readers park here.
- `state: AtomicU8` with `FILL_PENDING → FILL_SUCCESS | FILL_FAILED`.

`impl CancellableOp` → on issuer death the handle is removed from `in_flight`
and transitioned to `FILL_FAILED` so waiters retry.

### `InodePages::in_flight`

`kernel/src/fs/page_cache.rs:219`. `IrqSpinlock<BTreeMap<u64,
Arc<PageFillHandle>>>` at lock rank 42 (`RANK_IN_FLIGHT`, between `pages(40)`
and `dirty_keys(50)`). `IrqSpinlock` — not `BlockingMutex` — because
`CancellableOp::cancel` runs on the reaper kthread, which must not park
(drop-contract). Hold time is O(log N) get + remove.

### Reader state machine

`get_or_fill_async_sync(&Arc<VfsInode>, page_idx, fill_fn)` at `page_fill.rs`:

1. **Fast path**: grab `pages(40)`, probe for `page_idx`, return `PageGuard` on
   hit.
2. **Slow path (join)**: grab `in_flight(42)`, probe for `page_idx`. If
   present, `Arc::clone` the handle, drop `in_flight`, park on
   `handle.waiters` until `state != FILL_PENDING`. On wake:
   - `FILL_SUCCESS` → re-probe `pages(40)`, return.
   - `FILL_FAILED` → retry from step 1 (increments `INFLIGHT_RETRIES`).
3. **Publisher path**: install a new `Arc<PageFillHandle>` under
   `in_flight(42)`, drop the lock, call `fill_fn` (may hit AHCI), then:
   - Success: under `pages(40)` insert the produced `Arc<CachedPage>` →
     `handle.finish_success()` (wakes waiters) → remove from
     `in_flight(42)` → `owned_ops_remove`.
   - Failure: `handle.finish_failed()` (wakes waiters) → remove from
     `in_flight(42)` → `owned_ops_remove`.

Bulk fills (`get_or_fill_bulk_async_sync`) install one handle at every key in
`[start, start + page_count)` so partial joiners synchronize on the same
object. Cancel iterates the full range for removal — a single-index remove
would strand bulk waiters.

### Publish-terminal-state-before-remove invariant

Both `finish_success` and `finish_failed` MUST be called BEFORE removing from
`in_flight`. Removing first opens a window where:

- Waiter wakes, re-checks `in_flight` → empty, re-checks `pages` → empty on
  failure.
- Waiter races to install a new handle.
- The old `finish_failed` fires after the new handle is installed — the stale
  `wake_all` wakes the new handle's waiters spuriously.

Ordering is enforced in `get_or_fill_async_sync`; the rule is documented at
`page_fill.rs:17`.

### Lock-order proof sketch

- Fast path: `pages(40)` alone. Leaf.
- Slow path: `in_flight(42)` alone → park outside all InodePages locks.
- Publisher path: `in_flight(42)` alone to install → drop → `fill_fn` runs
  with NO InodePages lock held → `pages(40)` to publish → `in_flight(42)` to
  remove. Two disjoint scopes, no nested `40 → 42` hold.

`fill_fn` reaches BPC (110), journal (120–150), EFS `mutable` (160), AHCI
(170–200), deep utility leaves (900/910). All outside any InodePages lock.
Validated against the rank table in `doc/invariants/lock-order.md`.

### Drop contract

`PageFillHandle::drop` is non-blocking (no WaitQueue wait_until inside Drop).
`cancel` runs on the reaper path and only touches `IrqSpinlock` (non-parking) +
`AtomicU8::store` + `WaitQueue::wake_all`. Registered as an `owned_op` on the
issuing thread so that if the issuer dies mid-fill, the reaper drains the op
and transitions the handle to `FAILED` so waiters retry cleanly.

## Phasing and commits

| Phase | Commit | Content |
|------:|:-------|:--------|
| 0 | `611fc2a` | Audit doc + `FrameDrop` RAII wrapper |
| 1 | `fe8da51` | `PageFillHandle` + `in_flight` field + `get_or_fill_async_sync` free fn; `WAITQUEUE_CAP` 32→64 |
| 2 | `30016ad` | Migrate `page_cache_read`, `page_cache_write`, `get_or_fill_page`, `loader::prefetch_file_pages` to new entry points + `get_or_fill_bulk_async_sync` |
| 3 | `cddce8f` | Delete legacy `InodePages::get_or_fill`, clean module doc |
| 4 | `370c31e` | `INFLIGHT_INSTALLS/JOINS/RETRIES/CANCELS/CURRENT` counters + `/proc/inflight_stats` + `programs/inflighttest` |
| 5 | (this doc) | Validation + post-mortem + lock-order.md update + ideas.txt |

Bug fixes caught during the session (see Edge cases below) landed between
phases and are listed separately.

## Edge cases and bugs caught

### Orphan waiters in `!push_ok` path (commit `b9a6987`)

Initial `get_or_fill_async_sync` on a push failure (heapless overflow on the
in-flight map) removed the handle from `in_flight` without publishing a
terminal state first. A slow-path reader that had already cloned the handle in
the install window would park forever — no wake_all ever fires. Fix: order is
now `finish_failed` → remove, mirroring the success path.

### `PageFillHandle.len` u32 truncation (commit `8ef42a8`)

`page_count` at the bulk call sites is `u64`; storing it into a `u32` `len`
field via `as u32` silently truncated. No current caller approaches
`u32::MAX`, but as a latent hazard on future large mmap-heavy workloads we
changed the field to `u64`.

### Ring-0 demand fault with IRQs disabled (commit `725d46f`)

Phase 2 exposed a pre-existing latent bug: the ring-0 branch of the page-fault
handler at `interrupts/idt.rs` called `handle_demand_fault` with IRQs
disabled. The ring-3 branch already re-enabled them with an explanatory
comment; ring-0 was missed. Phase 2's increased FS concurrency made a BPC
shard mutex contend while held under a ring-0 demand-fault path, tripping the
`BlockingMutex::lock contended with interrupts disabled` panic from
Foundation #4's debugging. Fix mirrors the ring-3 branch: re-enable IRQs
across the demand-fault call.

### `emergency_println!` for KILL path (commit `9a6a3cf`)

Diagnostic work exposed a SERIAL_DBG deadlock window: when a ring-0 page
fault's KILL logs contended on `SERIAL_DBG`, and another CPU held it for a
long println, both CPUs spun with no visible holder. New
`serial::_emergency_print` + `emergency_println!` macro writes directly to
port 0x3F8, skipping the lock. KILL path uses it; future ring-0 faults can't
deadlock on the serial lock. Defense, not diagnosis.

### `block_page_cache: detached fallback` UART saturation (commit `2ca433b`)

During the AHCI NCQ investigation we noticed ~130/s × 120-char log lines on
"detached fallback" saturated the 115200-baud serial port, which throttled
the entire kernel via the serial spinlock and pushed drive latency past the
5 s AHCI NCQ timeout. Rate-limited to first + every 1000th hit. This was a
self-inflicted hang layered on top of the real NCQ stall, not a Foundation
#5 regression, but cleaning it up was a prerequisite for meaningful triage.

### Lock-order Arc guard (commit `147ab53`)

Defensive only: if `Arc::as_ptr(&thread)` is below the kernel half
(`0xffff_8000_0000_0000`), the rank enter/exit/enter_same calls skip
tracking and `emergency_write` a diagnostic. Papers over a `CR2=0x440` NPE
seen once during the session; root cause (suspected torn percpu
`current_thread` read) not found. If it fires again the emergency write will
point to the call site.

## Measured performance

Phase-5 boot with `inflighttest` (small-file variant): boot + `ls /bin` + shell
startup all green, no `/proc/lock_order_stats` inversions. `/proc/inflight_stats`
shows the expected shape on the test workload:

```
installs: N
joins: 0-N (higher under concurrent read of the same uncached file)
retries: 0 in clean runs
cancels: 0 in clean runs
current: 0 after test completion
```

No regression vs. the ~200ms Foundation #4 mmaptest baseline. The backend is
still sync (one `fill_fn` call = one blocking AHCI read chain), so no
asymptotic change is expected at this layer. The async win is Foundation #6's
to deliver, via the generic async block I/O trait that swaps `fill_fn` for a
submit/wait pair.

## What this unblocks

- **Foundation #6 — generic async block I/O trait.** The split submit/wait
  API can hook directly into `fill_fn`: the publisher installs the handle,
  fires submit, parks on a completion-thread wake; no new dedup or state
  required at the page-cache layer.
- **Async NCQ readahead** (`.claude/plans/async-ncq-readahead.md`). Its
  original design assumed a page-level "fill in flight" state that did not
  exist. Foundation #5's `in_flight` map is exactly that. The original plan's
  review blockers that traced to page-cache concurrency are resolved;
  Foundation #6 will re-validate the remaining AHCI-side blockers when that
  work starts.
- **Concurrent read amplification** — previously two user threads reading
  the same uncached file issued duplicate AHCI reads. Now the second reader
  joins the existing handle. Quantitative win unmeasured; surfaces as
  `INFLIGHT_JOINS > 0` in workloads with shared file access.

## Open items

- Phase 5 Task 5.1 user-observed full mmaptest timing is a boot test; this
  doc records the qualitative result ("boots fine, no regression") per user
  confirmation. Re-record exact timings if Foundation #6 lands changes that
  warrant comparison.
- AHCI NCQ timeout on 8 MiB + fsync `inflighttest` is tracked separately in
  `.claude/handoff/2026-04-17-3-foundation-5-phase-4-plus-ahci-ncq-hang.md`,
  not a Foundation #5 regression (kernel instrumented; waiting on triage of
  io_uring vs qcow2 vs kernel-side).
