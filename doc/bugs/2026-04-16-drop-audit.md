# Drop Audit: Types reachable from `Thread::free`

**Date:** 2026-04-16
**Context:** Foundation #2 — Cancellable/transferable waits for dying threads.
**Purpose:** Enumerate every `Drop` impl reached from `Thread::free` (direct or transitive),
classify whether it is blocking, and decide the action.

---

## Audit table

| Type | Drop impl location | Blocking? | Action |
|---|---|---|---|
| `VfsInode` | `fs/inode.rs:85` | **Yes** (calls `evict_inode`, which does AHCI I/O on EFS) | Defer via `EVICT_QUEUE` (Phase 1) |
| `CachedPage` | **None** (no Drop impl today) | N/A | Flagged: will gain Drop in Foundation #3; must follow drop-contract when it does |
| `CachedBlockPage` | `fs/block_page_cache.rs:153` | No (decrements refcount + frame dealloc, non-blocking) | Keep |
| `PageGuard` | `fs/page_cache.rs:124` | No (calls `CachedPage::unpin`, atomic decrement) | Keep |
| `TxHandle` | `fs/journal/tx.rs:58` | No (merges staged blocks into active tx under a `spin::Mutex`, no I/O) | Keep |
| `SharedMemory` | `memory/shared.rs:169` | No (frame dealloc under `IrqSpinlock`, no I/O) | Keep |
| `MemoryManager` | No explicit Drop impl | N/A | Keep |
| `FpuState` | No explicit Drop impl | N/A | Keep |
| `SignalState` | No explicit Drop impl | N/A | Keep |
| VMA (`Vma`) | No explicit Drop impl (resources freed in `Thread::free` arms) | N/A | Keep |
| `FileDescriptorTable` (pipe/pty/socket drain) | Handled explicitly in `Thread::free:716-782` | No (closes descriptors via `BlockingMutex` + `WaitQueue::wake_all`; non-I/O) | Keep |

---

## Key findings

### D1: VfsInode::drop can fire from live-process paths

Confirmed. At least three paths reach the final `Arc<VfsInode>` drop outside `Thread::free`:

1. `syscalls/memory.rs:670-686` — `munmap` drops the `FileBacked` VMA; if this was the
   last Arc for an orphaned inode, `VfsInode::drop` fires synchronously on the munmapping thread.
2. `fs/vfs.rs` — dentry detach in `remove_file`/`remove_dir` drops the dentry's Arc; if
   no file handles or VMAs remain, `VfsInode::drop` fires on the FS thread.
3. `thread/thread.rs:825-863` (Thread::free) — the `FileBacked` VMA arm drops the Arc
   at the bottom of the match block; if orphaned and last ref, eviction fires on the reaper.

All three are addressed by changing `VfsInode::drop` to call `post_evict` instead of
calling `fs.evict_inode` directly (Phase 1).

### D2: Worst-case orphan-drop burst

Typical: 0-2 per process death. Pathological (mmap+unlink N files then die): up to N.
Queue capacity 256 (D3) covers any realistic case. The WARNING fallback on queue-full
reintroduces a brief blocking path but never loses an eviction (on-disk space leak
otherwise).

### D3: AHCI slot call sites

| Function | File:approximate-line | Uses `allocate_slot_blocking`? |
|---|---|---|
| `ncq_read` | `drivers/ahci/port.rs:~857` | Yes |
| `ncq_write_inner` | `drivers/ahci/port.rs:~925` | Yes |
| `ncq_read_batch` | `drivers/ahci/port.rs:~1009` | Yes (per slot) |
| `non_ncq_read` | `drivers/ahci/port.rs:~1107` | No (`allocate_slot`) |
| `non_ncq_write` | `drivers/ahci/port.rs:~1174` | No (`allocate_slot`) |
| `flush_cache` | `drivers/ahci/port.rs:~1381` | No (`allocate_slot`) |
| `identify_device` | `drivers/ahci/port.rs:~1238` (execute_ata_identify) | No (`allocate_slot`) |
| `atapi_read` | `drivers/ahci/port.rs:~1444` | No (`allocate_slot`) |

Reference for Phases 3a/3b.

### D4: USB mailbox analysis (Task 0.5)

The USB block I/O path (`block_api.rs:usb_read_sectors/usb_write_sectors`) calls
`mailbox.send(...)` which returns a `Response<R>` backed by `Arc<ResponseInner<R>>`.
The caller parks on `response.wait()` via a `WaitQueue`.

**What happens if caller dies while parked:**

- The xHCI driver thread retains `Arc<ResponseInner<R>>` via the in-flight `Request`.
- When the driver calls `req.reply(...)`, it writes the result into `ResponseInner.value`
  and wakes the `WaitQueue`. Per Foundation #1, `wake_thread` on a dead thread is a no-op.
- The `Response<R>` (owned by the dead caller's stack frame) will have been dropped by
  `Thread::free`; `Arc<ResponseInner>` strong-count drops to 1 (driver's copy in Request).
- The driver's `Request::reply` call drops the `Request`, dropping `Arc<ResponseInner>` to 0.
  The `UsbBlockResponse::ReadResult(Vec<u8>)` inside drops. No DMA buffer lifetime issue:
  the `Vec<u8>` passed to `UsbBlockRequest::Read` is moved into the Request, owned by the
  driver. The caller's buffer is not a DMA target after `send()`.

**Conclusion:** No cancellation wrapper needed for USB block I/O. The data lifecycle is
entirely inside the xHCI driver thread; caller death causes the response to drop silently.
This is documented here and in a comment in `block_api.rs` (Phase 4 scope, Session B).

---

## Forward note

`CachedPage` has no `Drop` impl today. Foundation #3 will add one (page eviction from
the inode page cache). When that lands, the Drop must follow the drop-contract:
non-blocking, no AHCI I/O, no `BlockingMutex::lock`. Post to a kthread if needed.
See `doc/invariants/drop-contract.md` for the canonical contract text.
