# Drop-Contract Invariant

**Scope:** All `Drop` implementations on types reachable (directly or transitively)
from a dying thread's data structures — including types reachable from `Thread::free`,
from `VfsInode::drop`, and from any driver resource type that a thread owns.

---

## The invariant

> **Drop implementations on types reachable from a dying thread must be non-blocking.**

Concretely, a conforming `Drop` must NOT:

- Issue AHCI / USB I/O (neither blocking nor interrupt-driven waits).
- Acquire a `BlockingMutex` or call `WaitQueue::wait_until`.
- Call `thread_park_while` or any other sleep primitive.
- Call `fs.evict_inode` directly (goes through AHCI on EFS).

If a `Drop` needs blocking work, it must post a descriptor to a dedicated kthread
and return immediately.

---

## Reference pattern: `VfsInode::drop` → `EVICT_QUEUE`

`VfsInode::drop` (at `kernel/src/fs/inode.rs`) is the canonical example:

1. The drop site captures `(mount_id, ino)` — two `Copy` fields, no blocking work.
2. It calls `fs::evict::post_evict(mount_id, ino)` which pushes an `EvictRequest`
   onto a `crossbeam_queue::ArrayQueue<EvictRequest>`.
3. The `evict-inode` kthread (spawned at boot in `init_evict_kthread`) drains the
   queue and performs the potentially-blocking `fs.evict_inode` call.
4. If the queue is full: synchronous fallback with a WARNING log (never lose an eviction),
   or panic if the caller is the evict kthread itself (recursive runaway — see D8 in the
   Foundation #2 plan).

---

## Types this contract applies to

| Type | Status | Notes |
|---|---|---|
| `VfsInode` | Conforming (Phase 1) | Uses `post_evict` kthread queue |
| `CachedPage` | **No Drop today** | Will gain Drop in Foundation #3; must conform |
| `CachedBlockPage` | Conforming | Frame dealloc only; non-blocking |
| `PageGuard` | Conforming | `CachedPage::unpin` is atomic; non-blocking |
| `TxHandle` | Conforming | Merges into active journal tx under spin lock; no I/O |
| `SharedMemory` | Conforming | Frame dealloc under `IrqSpinlock`; non-blocking |
| `MemoryManager` | Conforming | No Drop impl; cleanup done explicitly in `Thread::free` |
| `FpuState` | Conforming | No Drop impl |
| `SignalState` | Conforming | No Drop impl |
| VMA (`Vma`) | Conforming | No Drop impl; resources freed in `Thread::free` arms |
| `FileDescriptor` variants | Conforming | Closed explicitly in `Thread::free:716-782` via `wake_all`; no I/O |

---

## Enforcement (debug builds)

Phase 5 of Foundation #2 adds `debug_assert!` guards at blocking Drop sites:

```rust
debug_assert!(
    !current_thread_is_reaper() && !current_thread_is_evict_kthread(),
    "blocking Drop fired on reaper or evict kthread"
);
```

These are compiled out in release. They catch regressions at development time
before they serialise production exits.

---

## How to add a new type

If you add a `Drop` impl to a type that may be reachable from a dying thread:

1. Determine whether the body is blocking (I/O, mutex acquire, park).
2. If non-blocking: add the type to the table above as "Conforming".
3. If blocking: follow the `EVICT_QUEUE` pattern — extract a `Copy` descriptor,
   push to a dedicated kthread queue, return immediately from `Drop`.
4. Update this file and `doc/bugs/2026-04-16-drop-audit.md`.

---

*See also:* `doc/bugs/2026-04-16-drop-audit.md` for the initial audit table and
per-path reachability analysis.
