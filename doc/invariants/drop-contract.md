# Drop-Contract Invariant

Sibling document: [`lock-order.md`](lock-order.md).

> **Drop implementations on types reachable from a dying thread must be
> non-blocking.**

Scope: every `Drop` reachable, directly or transitively, from a dying thread's
data structures. That includes anything reachable from `Thread::free`, from
`VfsInode::drop`, and from any driver resource a thread owns.

A conforming `Drop` must not:

- issue AHCI or USB I/O, blocking or interrupt-driven;
- acquire a `BlockingMutex`, or call `WaitQueue::wait_until`;
- call `thread_park_while` or any other sleep primitive;
- call `fs.evict_inode` directly, which reaches AHCI on EFS.

If a `Drop` needs blocking work, it posts a descriptor to a dedicated kthread and
returns immediately.

---

## Reference pattern: `VfsInode::drop`

`VfsInode::drop` in `fs/inode.rs` is the canonical shape:

1. Capture `(mount_id, ino)`, two `Copy` fields. No blocking work.
2. Call `fs::evict::post_evict(mount_id, ino)`, which pushes an `EvictRequest`
   onto a `crossbeam_queue::ArrayQueue`.
3. The `evict-inode` kthread, spawned at boot by `init_evict_kthread`, drains the
   queue and performs the blocking `fs.evict_inode` call.
4. If the queue is full: fall back to a synchronous evict with a warning, so an
   eviction is never lost. If the caller *is* the evict kthread, panic instead;
   that path is a recursive runaway.

---

## Conformance

Every `Drop` impl in the kernel, and why it conforms:

| Type | Location | Why it is non-blocking |
|---|---|---|
| `VfsInode` | `fs/inode.rs` | posts to the evict kthread |
| `CachedPage` | `fs/page_cache.rs` | frame dealloc only |
| `CachedBlockPage` | `fs/block_page_cache.rs` | frame dealloc only |
| `PageGuard` | `fs/page_cache.rs` | `unpin` is a single atomic |
| `BlockPageGuard` | `fs/block_page_cache.rs` | `unpin` is a single atomic |
| `FrameDrop` | `memory/frame_drop.rs` | frame dealloc only |
| `SharedMemory` | `memory/shared.rs` | frame dealloc under `IrqSpinlock` |
| `TxHandle` | `fs/journal/tx.rs` | merges into the active tx under a spin lock, no I/O |
| `UAccessGuard` | `util/uaccess.rs` | clears a per-CPU flag |

Types with no `Drop` at all, freed explicitly in `Thread::free`: `MemoryManager`,
`FpuState`, `SignalState`, `Vma`, and the `FileDescriptor` variants (closed via
`wake_all`, no I/O).

---

## Enforcement

Debug builds assert that a blocking `Drop` never fires on a thread that must not
block. Two helpers, both `#[inline]` and allocation-free, and both `false` before
their kthread starts:

| Helper | Location |
|---|---|
| `current_thread_is_reaper()` | `thread/scheduler.rs` |
| `current_thread_is_evict_kthread()` | `fs/evict.rs` |

`VfsInode::drop` carries the guard, because it was blocking historically and is the
most likely place for a regression to land:

```rust
#[cfg(debug_assertions)]
{
    debug_assert!(
        !crate::thread::scheduler::current_thread_is_reaper()
            && !crate::fs::evict::current_thread_is_evict_kthread(),
        "blocking Drop (TypeName) fired on reaper or evict kthread"
    );
}
```

The other types need no guard while their drops stay trivial. Guards compile out
in release builds; they exist to catch a regression during development, before it
serialises production exits.

---

## Adding a `Drop`

If you add a `Drop` to a type reachable from a dying thread:

1. Decide whether the body blocks (I/O, mutex acquire, park).
2. Non-blocking: add a row to the conformance table above.
3. Blocking: follow the evict-queue pattern. Extract a `Copy` descriptor, push it
   to a dedicated kthread queue, return immediately, and add the guard.
