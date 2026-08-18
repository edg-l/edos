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
   eviction is never lost. That fallback issues disk I/O, so it is only legal on
   a thread that may block:
   - on the evict kthread it is a recursive runaway, and panics;
   - on the reaper it would stall every process teardown behind the I/O, so the
     eviction is abandoned, counted in `/proc/evict_stats` as `dropped_count`,
     and logged. The inode's blocks stay allocated until `efs-fsck` reclaims
     them, which is far cheaper than blocking teardown.

**Running on the reaper is expected, not a violation.** The reaper frees a dead
thread's descriptors and VMAs, so it routinely drops the last reference to an
orphaned inode; that is the exact case this pattern exists to make safe. The
contract is that the drop never *blocks*, not that it never *runs* in those
contexts, so the guard belongs on the blocking fallback rather than on `Drop`
itself. A guard on `Drop` forbids the very path the design guarantees will
happen — `mmaptest`'s unlink-while-mapped case panicked the kernel on it.

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
| `BlockBuffer` | `drivers/block_io.rs` | one `Weak::upgrade` and one atomic decrement |

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

The helpers gate the *blocking* work, not the drop. `post_evict` uses them to
decide what the queue-full fallback may do:

```rust
if current_thread_is_evict_kthread() {
    panic!("recursive orphan drop runaway");
}
if current_thread_is_reaper() {
    // Cannot block here; give the eviction up and let fsck reclaim.
    EVICT_DROPPED_COUNT.fetch_add(1, Ordering::Relaxed);
    return;
}
// Any other thread may block.
fs.evict_inode(ino)
```

Guarding `Drop` itself with these helpers is wrong: it fires on the legitimate
path where the reaper releases the last reference to an orphaned inode. Put the
check where the blocking call is.

---

## Adding a `Drop`

If you add a `Drop` to a type reachable from a dying thread:

1. Decide whether the body blocks (I/O, mutex acquire, park).
2. Non-blocking: add a row to the conformance table above.
3. Blocking: follow the evict-queue pattern. Extract a `Copy` descriptor, push it
   to a dedicated kthread queue, return immediately, and add the guard.
