# Foundation #2: Cancellable/Transferable Waits — Post-Mortem

**Date:** 2026-04-16 through 2026-04-17
**Sessions:** A (Phases 0-2), B (Phases 3a-3b-4), C (Phases 5-6)
**Status:** Complete (Phases 0-6 shipped)

---

## Motivation

Three related correctness hazards existed before this work:

1. **Blocking reaper**: `Thread::free` → `VfsInode::drop` → `fs.evict_inode` could issue
   AHCI I/O synchronously on the reaper kthread. Every process death serialised behind disk I/O.

2. **AHCI slot leak on thread death**: Each `slot_waiters[i]` stored `Weak<Thread>`.
   If the submitter died while parked, the IRQ wake would call `wake_thread` on a dead weak
   handle (Foundation #1 no-op). The post-park cleanup that freed the slot would never run.
   The slot leaked until port reset. With 32 NCQ slots, a pathological workload (32 threads
   each doing NCQ reads and dying) could exhaust all slots permanently.

3. **No per-thread in-flight operation registry**: Nothing enumerated a thread's async ops
   on death, so there was no systematic way to cancel them.

---

## Design

### Deferred eviction (Phase 1)

`VfsInode::drop` was rewritten to call `fs::evict::post_evict(mount_id, ino)` instead of
`fs.evict_inode` directly. A dedicated `evict-inode` kthread drains a
`crossbeam_queue::ArrayQueue<EvictRequest>` (capacity 256). If the queue is full and the
caller is NOT the evict kthread, it falls back to synchronous eviction with a WARNING log.
If the caller IS the evict kthread (recursive orphan drop with a full queue), it panics — a
loud signal for a runaway condition.

### Per-thread `owned_ops` registry (Phase 2)

`Thread` gained an `IrqSpinlock<heapless::Vec<Arc<dyn CancellableOp>, 32>>`. Submitters
push an op before parking; drivers (or submitters) remove it on normal completion; `Thread::free`
calls `owned_ops_cancel_all` before any other resource teardown.

### AHCI slot cancel (Phases 3a + 3b)

`slot_waiters[i]` type changed from `Option<Weak<Thread>>` to `Option<Arc<AhciSlotOp>>`.
`AhciSlotOp` carries an `AtomicU8` state machine (Pending/Completed/Cancelled) and implements
`CancellableOp`. The cancel path and the IRQ completion path race on this state; whoever CASes
first owns the slot release. `wake_all_slot_waiters` was rewritten to CAS before waking, and
to call `release_orphaned_slot` directly when the submitter is dead or dying.

Two submit helpers (`with_slot_blocking`, `with_slot_try`) consolidate all eight AHCI submit
sites into uniform registration + cleanup patterns.

### Drop-contract guardrails (Phase 5)

Runtime helpers `current_thread_is_reaper()` and `current_thread_is_evict_kthread()` (TID
comparison, alloc-free) are exposed from `scheduler.rs` and `fs/evict.rs` respectively.
A `debug_assert!` guard at `VfsInode::drop` fires in debug builds if either kthread drops
an orphaned inode — catching any future regression that re-introduces blocking code into the
drop path before the `post_evict` call.

---

## Edge cases encountered

### BLOCKER-2: IRQ completion wins CAS, waiter thread already dying

The IRQ path marks `SLOT_COMPLETED` and tries `wake_thread`. With Foundation #1, wake on a
dead thread is a no-op. But the post-park slot cleanup never runs either (the thread is dying).
Fix: after winning the `Pending→Completed` CAS, check if `waiter.upgrade()` returns `None`
or a `Dying` thread. In both cases, call `release_orphaned_slot` directly. The cancel path
then loses its own CAS to `COMPLETED` and calls `maybe_release_slot`, which is a no-op since
the slot was already freed by the IRQ path.

### `ArcCancellableOp` `Err(COMPLETED)` branch in `AhciSlotOp::cancel`

When the completion path wins `Pending→Completed` and then discovers a dying thread (calling
`release_orphaned_slot`), the cancel path's CAS fails with `Err(COMPLETED)`. The cancel path
must still check whether the slot was freed. `maybe_release_slot` does a conditional release
(only if the same `AhciSlotOp` is still in `slot_waiters`) to avoid a double-free if the IRQ
path already cleaned up.

### `ncq_read_batch` + OWNED_OPS_CAP

`ncq_read_batch` issues up to 31 concurrent NCQ slots. With `OWNED_OPS_CAP = 32`, overflow
is possible when a thread has one prior op registered. On overflow, the slot proceeds without
cancel hookup (pre-Foundation-#2 behaviour for that slot) and logs a debug warning. Setting
the cap to 32 was a deliberate trade-off (D6): it covers the common case; the corner case
degrades gracefully.

---

## Performance impact

- **Hot path** (`owned_ops_push` before park, `owned_ops_remove` after wake): two
  `IrqSpinlock` lock/unlock operations per AHCI command round-trip. Lock is uncontended in
  the common case (no concurrent cancel). Cost is negligible vs. disk latency.
- **`Thread::free`**: `owned_ops_cancel_all` drains up to 32 entries. Each `cancel()` call
  is a CAS + conditional `release_orphaned_slot` (slot bitmask atomic OR). Constant time.
- **`VfsInode::drop`**: `post_evict` is a single `ArrayQueue::push` + `wake_thread`. This
  replaces a synchronous AHCI I/O call. The reaper now unblocks instantly.

No `dd`-style throughput regression is expected. The lock-acquisition overhead is in the
microsecond range; AHCI NCQ commands take milliseconds.

---

## Files changed (summary)

- `kernel/src/fs/inode.rs` — `VfsInode::drop` rewritten; debug_assert guard added
- `kernel/src/fs/evict.rs` — new: deferred eviction kthread, `post_evict`, `EVICT_TID`,
  `current_thread_is_evict_kthread`, `EVICT_DRAIN_COUNT`, `evict_kthread_drain_count`
- `kernel/src/fs/mod.rs` — `pub mod evict` registered
- `kernel/src/thread/thread.rs` — `owned_ops` field, three helper methods, call to
  `owned_ops_cancel_all` in `Thread::free`
- `kernel/src/thread/cancel.rs` — new: `CancellableOp` trait, `OWNED_OPS_CAP`, `ArcCancellableOp`
- `kernel/src/thread/cancel_smoke.rs` — synthetic + AHCI slot cancel smoke tests
- `kernel/src/thread/mod.rs` — `pub mod cancel`, `pub mod cancel_smoke`, re-exports
- `kernel/src/thread/scheduler.rs` — `REAPER_TID`, `current_thread_is_reaper`, stored in
  `init_reaper`
- `kernel/src/drivers/ahci/cancel_op.rs` — new: `AhciSlotOp`, state machine constants
- `kernel/src/drivers/ahci/port.rs` — `slot_waiters` type, `weak_self`, `with_slot_blocking`,
  `with_slot_try`, `release_orphaned_slot`, `maybe_release_slot`, `wake_all_slot_waiters` rewrite;
  `alloc_slot_for_cancel_test` + `free_slots_mask` test helpers (cancel-smoke feature)
- `kernel/src/drivers/ahci/mod.rs` — `set_weak_self` call after `Arc::new(port)`
- `kernel/src/drivers/ahci/direct.rs` — `first_ata_port` test helper (cancel-smoke feature)
- `kernel/src/drivers/usb/block_api.rs` — comment: USB caller-death is safe by design
- `kernel/src/main.rs` — `init_evict_kthread` wiring; `ahci_cancel_smoke_kthread` for tests
- `doc/invariants/drop-contract.md` — updated: runtime guards, helper locations, type table
- `doc/bugs/2026-04-16-drop-audit.md` — initial drop audit table
- `doc/bugs/2026-04-16-foundation-2.md` — this file

---

## Tests not implemented (with rationale)

- **Task 6.1** (orphan mmap test): requires kernel VFS orchestration (create/mmap/unlink file
  from a kthread, then exit and verify evict fires). Complex; the plan marks it optional and
  the Task 6.0 baseline covers the cancel path. The `evict_kthread_drain_count` accessor is
  in place for a future integration test.
- **Task 6.2** (synthetic SIGKILL via debug syscall): out of scope per plan (signals not yet
  wired). Plan explicitly says skip if complex.
- **Task 6.3** (EFS integration check): requires mounted EFS. The drain counter is ready;
  the test can be added as part of a broader EFS test suite.
- **Task 6.4** (32-thread stress): depends on 6.2 per plan; skipped automatically.
