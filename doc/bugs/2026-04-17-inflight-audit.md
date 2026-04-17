# Foundation #5 — Pre-implementation audit

Date: 2026-04-17. Produced by Phase 0 tasks.

---

## Task 0.1: `get_or_fill` / `get_or_fill_page` callsite table

| # | File:line | Lock context at entry | Call pattern | Failure semantics |
|---|-----------|----------------------|--------------|-------------------|
| 1 | `fs/vfs.rs:324` | `inode.lock.read(30)` held (via `vfs::read` → `page_cache_read`) | Bulk-fill distribution loop: per-page inside `for page_idx in range_start..=range_end` after `fill_pages_bulk` succeeded | Propagates `Err` to `page_cache_read` caller |
| 2 | `fs/vfs.rs:340` | `inode.lock.read(30)` held | Fallback per-page fill loop when `fill_pages_bulk` returns `Err` | Propagates `Err` to `page_cache_read` caller |
| 3 | `fs/vfs.rs:377` | `inode.lock.read(30)` held | Result-collection loop after readahead pre-fill; expected cache hit, still calls `fill_page` on miss | Propagates `Err` to `page_cache_read` caller |
| 4 | `fs/vfs.rs:528` | `inode.lock.read(30)` held (via `vfs::write` → `page_cache_write`) | Full-page overwrite path — zero-fills a new page into cache | Propagates `Err` to `page_cache_write` caller |
| 5 | `fs/vfs.rs:533` | `inode.lock.read(30)` held | Partial-page write path — reads existing page to merge with new data | Propagates `Err` to `page_cache_write` caller |
| 6 | `fs/vfs.rs:1023` | No inode.lock held (free function, called from fault.rs and loader) | Single-page fill via `PageCacheOps::fill_page`; wraps `InodePages::get_or_fill` | Returns `Err(EINVAL/EIO)` to caller |
| 7 | `loader/mod.rs:172` | No inode.lock held (pre-fetch helper, called before ELF load) | `prefetch_file_pages`: per-page loop after bulk fill; ignores errors (prefetch is best-effort) | Ignores error via `let _ = ...` |
| 8 | `loader/mod.rs:109` | No inode.lock held (reloc-page copy in `copy_elf_page`) | Single page for ELF relocation COW copy | Maps to `ElfLoadError::MappingFailed` |
| 9 | `memory/fault.rs:163` | No inode.lock held, vmas(70) already released before this call | Single-page fault-in via `get_or_fill_page` (callsite #6 above) | Returns `FaultOutcome { mapped: false }` |
| 10 | `memory/fault.rs:322` | No inode.lock held, vmas(70) already released | Relocation fault path via `get_or_fill_page` | Returns `Some(false)` |

**Confirmed**: no callsite holds `inode.pages.pages` lock across the call to
`get_or_fill`. The `ranked_lock!(RANK_PAGES,...)` scope inside `get_or_fill`
itself is entered and exited in the fast-path; the slow path runs `fill_fn`
with no InodePages lock held.

**Confirmed**: none of the callsites runs in IRQ context. All are thread
context (scheduler running, park is safe).

---

## Task 0.2: `fill_page` / `fill_pages_bulk` reentrancy

Checked three implementations:

### EFS (`fs/efs/mod.rs:2480`, `:2539`)

`fill_page`: calls `self.read_inode(ino)` (acquires `EfsDriver.mutable` rank
160, reads inode data, releases), then `ahci::direct::read_sectors` (acquires
AHCI legacy_lock/slot_waiters/mmio_lock ranks 170-190). All state is in
`&self` (shared reference) or local variables. No per-call mutable state stored
on `&self` between invocations. Concurrent calls for different pages/inodes
proceed without blocking each other.

`fill_pages_bulk`: same pattern; reads a contiguous range via
`read_inode` + `direct::read_sectors`. No caller-specific state on `&self`.

**Conclusion: Reentrant. Safe for concurrent callers.**

### memfs (`fs/memfs/mod.rs:520`)

`fill_page`: reads `self.inner.read()` (spin RwLock, short hold), copies bytes
from node content into `buf`, releases. No mutable state on `&self`. Pure
memory copy. Zero blocking.

**Conclusion: Reentrant. Safe for concurrent callers.**

### FAT32 (`fs/fat32/page_cache.rs:167`)

`fill_page`: calls `self.lookup_inode_entry(ino)` (cluster chain walk) then
AHCI reads. All result state in local variables. No per-call state on `&self`.

**Conclusion: Reentrant. Safe for concurrent callers.**

---

## Task 0.3: WaitQueue reader-joining patterns audit

### `drivers/ahci/port.rs::wait_for_ncq_completion` (line 1097)

Pattern: **not** a WaitQueue-based join. Uses a raw `thread_park_while` loop
polling hardware SACT register in the closure. The NCQ slot itself is the
serialization unit. Concurrent readers for different slots proceed
independently; a slot error calls `wake_all_slot_waiters()` (a
`WaitQueue`-based wake of all slot owners, not a shared-publisher join).

Relevance to Foundation #5: AHCI uses raw park, not WaitQueue. The
`PageFillHandle::waiters: WaitQueue` pattern is new and does not conflict.

### `drivers/usb/block_api.rs`

Pattern: mailbox-based request/response. Each caller sends a request and
calls `response.wait()` (per-response WaitQueue with one waiter). Not a
shared-publisher join. Each USB I/O op has its own `ResponseInner::waitq`.

Relevance to Foundation #5: no shared-publisher reader-joining pattern in USB.
The `PageFillHandle` is the first instance of "N readers park on one publisher
handle" in this codebase.

### `thread/broadcast.rs`

Pattern: `Subscriber::recv()` calls `thread_park_while(|| self.queue.is_empty())`
on the subscriber's private queue. Broadcast sends to all subscribers. Not a
shared-publisher WaitQueue join; each subscriber has its own queue.

Relevance to Foundation #5: no comparable pattern. Foundation #5 introduces the
first true shared-publisher WaitQueue join.

**Finding**: Foundation #5 `PageFillHandle::waiters` is novel. No existing
mirroring pattern; we are defining the pattern.

---

## Task 0.4: `OWNED_OPS_CAP = 32` sufficiency

Worst case: one thread performing a bulk NCQ read (`ncq_read_batch`) with 31
concurrent NCQ slots, each producing one `AhciSlotOp`. If the same thread
simultaneously owns one `PageFillHandle` op, that is 31 + 1 = 32. Exactly
at the cap.

Raising to 48 would give headroom, but the plan specifies: "Raise to 48 only
if the audit surfaces a scenario that exceeds." No such scenario found: the
NCQ cap is hardware-enforced at 32 slots; in practice a single thread rarely
issues all 32 NCQ slots simultaneously (NCQ interleaving is across multiple
in-flight reader threads).

**Conclusion: `OWNED_OPS_CAP = 32` is sufficient. No bump needed.**

---

## Task 0.4b: `WAITQUEUE_CAP` bump

`kernel/src/thread/waitqueue.rs:13` — bumped from 32 to 64 per Task 0.4b.
See commit.

Rationale: fault-storm scenarios on a many-core machine can bring many threads
to the same uncached page simultaneously. 64 gives 2x headroom over 32 at
negligible cost (~512 B per handle, amortized over millions of cache hits).
Real concurrent-reader count for a single page is 1-4 in typical workloads.

---

## Task 0.5: VfsInode Arc/Weak upgrade races with `post_evict`

`VfsInode::drop` fires when `Arc::strong_count` reaches 0. It checks `orphan`
and calls `evict::post_evict(mount_id, ino)` for orphans.

**Callers of `get_or_fill_page`** all hold a strong `Arc<VfsInode>` (passed as
`inode: &Arc<VfsInode>`). Since the caller holds the Arc, `strong_count >= 1`
and `VfsInode::drop` cannot fire while the caller is inside `get_or_fill`.

**`PageFillHandle.inode: Weak<VfsInode>`**: only upgraded in `cancel()`, which
is called from the reaper kthread when the owning thread dies. The reaper holds
no Arc to the inode. Upgrade may fail if all other Arcs were dropped after the
owning thread died but before cancel fired. This is the "inode already freed"
case documented in the plan's Architecture Decision A: `Weak::upgrade` returning
`None` is safe — the in_flight map was freed with the inode, and no waiter can
still hold this handle for that inode.

**Race with `post_evict`**: `post_evict` is called inside `VfsInode::drop`,
which fires only when the last Arc drops. The evict kthread runs `evict_inode`,
which deallocates on-disk blocks but does NOT touch the in_flight map (which
lives on the VfsInode itself, already dropped). No race: the in_flight map's
memory is owned by the VfsInode; it goes away with the last Arc drop; any
concurrent `Weak::upgrade` then returns `None`.

**Conclusion**: publish-and-remove in `get_or_fill_async_sync` always runs
with a live `Arc<VfsInode>` in hand (the caller passes it). The Weak in the
handle is only upgraded on cancel; if the inode is gone, upgrade returns
`None` and cleanup is correctly skipped. No race.

---

## FrameDrop need confirmed

Three spots in the publisher path can exit early between frame allocation and
`Arc<CachedPage>` wrapping:

1. `fill_fn` returns `Err`.
2. Issuer thread dies mid-fill (reaper drops stack).
3. Double-insert guard returns existing Arc before our insert.

In case 1 and 3, a local `FrameDrop` RAII on the publisher's stack calls
`frame_allocator().deallocate_frame(frame)` on drop, returning the frame.
In case 2, the reaper drops the issuer's stack frames, firing `FrameDrop::drop`
automatically. Without `FrameDrop`, case 2 leaks a physical frame permanently.

`FrameDrop::forget()` converts to `PhysFrame` for the success path where
`Arc::new(CachedPage::new(frame))` takes ownership; `CachedPage::drop` then
handles deallocation.
