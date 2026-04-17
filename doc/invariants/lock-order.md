# Lock-Order Invariant

**Foundation #4 — Phase 0**
Written: 2026-04-17. Sibling to `doc/invariants/drop-contract.md`.

---

## The invariant

> **Every thread that acquires two or more ranked locks must acquire them in
> strictly increasing rank order (lower rank first).**

Rank is an integer assigned per lock class (not per instance). Spacing is in
multiples of 10 so future insertions do not force renumbering.

Same-class / different-instance acquisition (e.g. two inodes in `vfs::rename`)
is permitted only in documented key-ordered patterns, via the future
`ranked_lock_same!` macro (Phase 1). The caller is responsible for the key
ordering; the rank system cannot detect wrong-key-order same-class deadlocks.

Non-ranked locks are listed separately below with justification.

---

## Canonical rank table

| Rank | Lock | Type | Location | Notes |
|-----:|------|------|----------|-------|
|  10 | `VFS` mount registry | `spin::RwLock<BTreeMap>` | `fs/vfs.rs:47` | Read-held across `fs.clone()`; no inner locks taken while held. |
|  30 | `inode.lock` (per-inode) | `thread::rwlock::RwLock<()>` | `fs/inode.rs:50` | Outer per-inode gate. Held across FS driver callbacks (memfs, efs, fat32). Inner: 35, 40, 50, 60, 70, 80, 85. |
|  35 | `dentry_cache.inner` | `BlockingMutex` | `fs/dentry.rs:31` | Acquired after `inode.lock` (always — see audit log). Leaf on the dentry side. |
|  40 | `inode.pages.pages` | `BlockingMutex<BTreeMap>` | `fs/page_cache.rs:163` | Inner: `dirty_keys` (50). NEVER held across disk I/O (get_or_fill drops before calling fill_fn). |
|  42 | `InodePages.in_flight` | *(not yet added)* | — | **Forward reference for Foundation #5.** When async readahead lands, in_flight goes here between `pages` (40) and `dirty_keys` (50). |
|  50 | `inode.pages.dirty_keys` | `BlockingMutex<Vec>` | `fs/page_cache.rs:164` | Leaf on the per-inode side. |
|  60 | `inode.mappers` | `BlockingMutex<Vec<Weak<...>>>` | `fs/inode.rs:56` | Inner: vmas (70), per-process mm (80) on target UserThreads during truncate. |
|  70 | `UserThread.vmas` | `Arc<spin::Mutex<VmaSet>>` | `thread/mod.rs:44` | Per-process VMA set. Inner: 80 per-process mm. See `fault.rs:406-433`. |
|  80 | `UserThread.memory_manager` | `Arc<spin::Mutex<MemoryManager>>` | `thread/mod.rs:43` | Per-process page table. Leaf for PTE walks. Acquired alone or inside vmas (70). **Distinct class from rank 85.** |
|  85 | kernel-global mapper | `IrqSpinlock<MemoryManager>` | `memory/mapper.rs:46` via `boot_info().memory_manager` | Accessed via `memory_mapper()`. Kernel-address-space edits only (kmap, device mapping). Never co-held with rank 80. |
|  90 | `FRAME_ALLOCATOR` | `IrqSpinlock<BitmapFrameAllocator>` | `memory/frame_allocator.rs:17` | True leaf. Brief hold; no inner locks. |
| 100 | `DIRTY_INODES` | `IrqSpinlock<Vec<Weak<VfsInode>>>` | `fs/vfs.rs:37` | Called from `register_dirty_inode` (rank 30 inode.lock path) and writeback kthread (no other lock held). Holds nothing else. |
| 110 | `BlockPageCache.shards[N]` | `BlockingMutex<ShardInner>` | `fs/block_page_cache.rs:295, 372` | NEVER held across disk I/O. Inner: `journals` (120), `write_lock` (140). Sibling of 130/150 (journal ranks) — never co-held. |
| 120 | `BlockPageCache.journals` | `BlockingMutex<BTreeMap>` | `fs/block_page_cache.rs:311` | Brief; returns `Arc<Journal>`. Leaf within the block cache subsystem. |
| 130 | `Journal.checkpoint_tracker` | `BlockingMutex<BTreeMap>` | `fs/journal/mod.rs:109` | **Sibling leaf (post Task 0.0 fix).** Never co-held with `state` (150) in any path. See "Journal inversion fix" below. |
| 140 | `CachedBlockPage.write_lock` | `BlockingMutex<()>` | `fs/block_page_cache.rs:76` | Serializes partial writers per page. True leaf. |
| 150 | `Journal.state` | `BlockingMutex<JournalState>` | `fs/journal/mod.rs:102` | **Sibling leaf (post Task 0.0 fix).** `tx.rs` closes state's scope before acquiring tracker; all other callers take state alone. See "Journal inversion fix" below. |
| 160 | `EfsDriver.mutable` | `BlockingMutex<EfsMutableState>` | `fs/efs/mod.rs:81` | Acquired by EFS callbacks under `inode.lock` (30). Inner: block_page_cache shards (110), journal state (150). |
| 170 | `AhciPort.legacy_lock` | `BlockingMutex<()>` | `drivers/ahci/port.rs:175` | Serializes non-NCQ commands. Nested: slot_waiters (180), mmio_lock (190 — via underlying path). |
| 180 | `AhciPort.slot_waiters[i]` | `spin::Mutex<Option<Arc<AhciSlotOp>>>` | `drivers/ahci/port.rs:154` | Brief per-slot. Never held across park or I/O. |
| 190 | `AhciPort.mmio_lock` | `spin::Mutex<()>` | `drivers/ahci/port.rs:136` | Very short raw MMIO RMW. True leaf. |
| 200 | `PCI_CONFIG_LOCK` | `spin::Mutex<()>` | `drivers/pci/config.rs:12` | Config-space RMW. Acquired alone; true leaf. |

### Rank 80 vs 85: per-process and kernel-global mappers

These are two distinct objects of the same `MemoryManager` type assigned
different ranks because they represent different address spaces:

- Rank 80 (`user.memory_manager`): per-`UserThread`, `Arc<spin::Mutex<MemoryManager>>`,
  for user-address-space PTE edits. Acquired inside vmas (70).
- Rank 85 (kernel-global via `memory_mapper()`): `IrqSpinlock<MemoryManager>` at
  `boot.rs:79`, for kernel-address-space edits (kmap, device memory). Acquired alone
  or inside frame_allocator paths.

They are NEVER co-held. Distinct ranks ensure any accidental co-acquisition panics
in debug builds.

### Journal state and tracker: sibling leaves (post Task 0.0)

Before Foundation #4 Phase 0 (Task 0.0), `fs/journal/mod.rs::is_safe_to_flush`
and `::advance_tail` held `checkpoint_tracker.lock()` (rank 130) while calling
`committed_seq()` which internally acquires `state.lock()` (rank 150). This was a
`130 -> 150` nested acquisition. Meanwhile, `tx.rs::TxHandle::drop` took them in
the opposite order sequentially (`state` then `tracker`). Classic AB/BA.

**Fix (Task 0.0 commit `0023897`):** Hoist `committed_seq()` calls to before
`checkpoint_tracker.lock()` at both sites. After the fix, state and tracker are
never co-held in any path. If any future caller tries to co-hold them in either
order, the Phase 2 rank tracker will panic.

---

## Non-ranked locks

These locks are excluded from the rank system with explicit rationale.

### `Thread.owned_ops` (`IrqSpinlock<heapless::Vec<..>>`, `thread/thread.rs:207`)

Acquired from many unrelated call sites: AHCI submission (inside various
driver contexts, potentially under ranked locks), completion, and thread
reaper (during `Thread::free`). Assigning any fixed rank produces false
positives: too low causes false violations when acquired under fs locks; too
high causes false violations when acquired bare.

True leaf by construction — audited in Task 0.3a:

| Call site | Context | Inner lock? | Parks? |
|-----------|---------|-------------|--------|
| `port.rs:588` (`with_slot_blocking`) | Before park; no ranked lock held | No | No |
| `port.rs:681` (`with_slot_try`) | Before I/O submission; no ranked lock held | No | No |
| `port.rs:1313` (batch read loop) | Before `wait_for_ncq_completion`; no ranked lock held | No | No |
| `port.rs:628/719/1353/1416` (remove after wake) | After park returns; no ranked lock held | No | No |
| `thread.rs:994` (`owned_ops_cancel_all`) | Reaper context (`Thread::free`); drains under lock, cancels outside | No | No |

All sites: `lock() -> mutate Vec -> drop()`. No inner locks, no park or sleep
inside the lock scope. Non-ranking is the correct model.

### `WaitQueue.inner` (`spin::Mutex<Deque<Weak<Thread>>>`, `thread/waitqueue.rs:27`)

Held only for `push_back` / `pop_front` inside `without_interrupts` micro-sections.
The park itself happens with this lock released. Adding a rank would fire on every
`wait_until` call — useless noise. Already covered by the existing `BlockingMutex::lock`
debug_assert at `thread/mutex.rs:49` (rejects contended acquires with interrupts disabled).

### `BlockingMutex.waiters`

Uses `WaitQueue` internally; same rationale as above.

### Scheduler `rq`, `sleepers`, `SCHEDULERS`

Internal to park/wake plumbing; never held across user code. Violations manifest as
deadlocks caught by existing park/wake assertions. Not a multi-lock participant.

### `SHARED_MEMORY_REGISTRY` (`spin::RwLock<BTreeMap>`, `memory/shared.rs:13`)

Audited in Task 0.4. The pattern in `syscalls/shm.rs`:
- `SharedMemory::get()` takes `REGISTRY.read()` and immediately clones the `Arc`,
  dropping the read guard before any vmas/mm acquisition.
- `SharedMemory::dec_ref()` may take `REGISTRY.write()` (when ref count reaches 0
  and region is destroyed), but only after `vmas.lock()` has already been dropped
  (the VMA is removed first, then dec_ref is called on the returned value).
- `SharedMemory::destroy()` takes `REGISTRY.write()` with no vmas/mm held.

Conclusion: registry is never co-held with vmas (70) or mm (80). Leave non-ranked.

### `FUTEX_REGISTRY`, `PORT_TABLE`, window registries, etc.

Leaf locks outside the fs/mm hot paths. Out of Phase 2 scope; can be added in
Phase 3 if something surfaces.

---

## Driver-internal locks (out of scope for Phase 2 ranking)

The following locks are internal to FS driver implementations. They are called
within the `FileSystem` trait callback methods, which are always invoked under
`inode.lock` (rank 30). They are not in multi-lock paths with each other.

| Lock | Driver | Notes |
|------|--------|-------|
| `Fatfs.write_lock` | fat32 | Serializes fat32 mutations. Called within inode.lock. |
| `Fatfs.inode_table` | fat32 | Per-fat32-instance side table. |
| `Fatfs.fs_info` | fat32 | FAT FS info sector cache. |
| `DevFs.shared` | devfs | Device registry. Acquired alone or within inode.lock. |
| `MemFs.inner` | memfs | Memory FS node tree. Acquired alone or within inode.lock. |

These will be assigned ranks in Phase 3 if the audit identifies an ordering
concern between them and the Phase 2 ranked set.

---

## How to add a new lock

1. Assign a rank. Use a multiple of 10 in the appropriate range; if you need to
   insert between two existing ranks, use an intermediate value (e.g. rank 42
   between 40 and 50). Update this table.
2. Add the lock to the rank table above with: rank, type, file:line, and a note
   on what inner locks may be acquired while it is held.
3. In Phase 2+, wrap every acquisition site with `ranked_lock!(rank, "name", ...)`,
   `ranked_read!(...)`, or `ranked_write!(...)`. For `IrqSpinlock`, use
   `lock_ranked(rank, "name")`. For `thread::rwlock::RwLock`, use
   `read_ranked(rank, "name")` / `write_ranked(rank, "name")`.
4. If the lock is a leaf (no inner ranked locks), document it as such.
5. If the lock participates in a same-class two-instance pattern (like `inode.lock`
   in `vfs::rename`), document the key-ordering invariant and use `ranked_lock_same!`
   at that site.
6. Rank spacing: 10 units per step is intentional. Do not compress spacing when
   inserting; if you run out of room, use 5-unit spacing and document why.

---

## Foundation #5 forward reference

Foundation #5 (per-inode async I/O registry) will add:

- `InodePages.in_flight` at rank 42: a `BlockingMutex<BTreeMap<u64, PageFillHandle>>`.
  This sits between `pages` (40) and `dirty_keys` (50).
- Per-page `PageFillHandle` waitqueue: non-ranked (WaitQueue semantics, see above).

The edge `pages (40) -> in_flight (42) -> dirty_keys (50)` must be validated against
the rank table before Foundation #5 ships.

---

## Audit log 2026-04-17

### Methodology

Ran `rg -n "\.lock()\|\.read()\|\.write()"` across `kernel/src/fs/`,
`kernel/src/drivers/ahci/`, `kernel/src/memory/`, `kernel/src/thread/mutex.rs`,
`kernel/src/thread/rwlock.rs`, and `kernel/src/syscalls/shm.rs`. Cross-checked
each multi-lock path against the rank table.

### Findings

**No new inversions found beyond the journal tracker/state issue (Task 0.0).**

Specific observations:

1. **Journal tracker -> state inversion (Task 0.0).** `mod.rs:321` and `mod.rs:361`
   took `checkpoint_tracker.lock()` then called `committed_seq()` (which takes
   `state.lock()`). Fixed by hoisting `committed_seq()` to before the tracker
   acquisition. Committed as `0023897`.

2. **Per-process mm (80) vs kernel-global mapper (85) are distinct classes.**
   `thread/mod.rs:43` is `Arc<spin::Mutex<MemoryManager>>` (per-process).
   `boot.rs:79` `boot_info().memory_manager` is `IrqSpinlock<MemoryManager>`
   (kernel-global). They are never co-held; assigned ranks 80 and 85
   respectively so any accidental co-acquisition panics in debug builds.

3. **Dentry cache rank confirmed (Task 0.3).** All four `dentry_cache().invalidate()`
   call sites in `vfs.rs` (lines 559, 589, 599, 627, 641, 642, 676, 677, 678)
   are called after the parent inode's `lock.write()` is already held. The
   `remove_dir` path (line 640) and `rename` path (line 675) both follow the
   same pattern. Rank 35 is correct.

4. **`owned_ops` non-ranked confirmed (Task 0.3a).** All six acquisition sites
   (push/remove in `port.rs:588, 628, 681, 719, 1313, 1353, 1416`; drain in
   `thread.rs:994`) hold no other ranked lock, take no inner lock, and do not
   park. Non-ranked classification is sound.

5. **`SHARED_MEMORY_REGISTRY` confirmed non-ranked (Task 0.4).** No path in
   `syscalls/shm.rs` holds the registry lock while taking vmas (70) or mm (80).
   The pattern is: obtain `Arc<SharedMemory>` (drops registry guard), then
   acquire vmas/mm separately.

6. **Latent hazard, not an inversion: `DIRTY_INODES` (100) from two contexts.**
   `register_dirty_inode` (vfs.rs:793) is called from `page_cache_write` (under
   `inode.lock` rank 30, giving edge 30->100) and from `fault.rs` after the
   fault path drops vmas and mm (giving bare 100 acquisition). Both are
   monotonically consistent with rank 100.

7. **Deep EFS chain is monotone.** The chain `inode.lock (30) -> EfsMutableState
   (160) -> BPC.shards (110) -> journal.state (150) -> AhciPort.legacy_lock
   (170) -> mmio_lock (190)` is monotonically increasing throughout.

8. **block_page_cache.rs:790 takes `checkpoint_tracker` inside shard (110).**
   `flush_dirty_once` (around line 788) takes `journals.lock()` (120) inside
   `shards[si].lock()` (110), then briefly takes `j.checkpoint_tracker.lock()`
   (130) inside the journals scope. Order: 110 -> 120 -> 130 (monotone, fine).
   The `state.lock()` that appears on line 839 is in a separate scope, after
   the shard is re-acquired fresh, not nested with tracker. Post-Task-0.0 this
   is clean.

### Additions to rank table from audit

None. All discovered lock sites are either:
- Already in the table (EFS, AHCI, BPC).
- Internal FS driver locks (fat32, devfs, memfs) that are out of Phase 2 scope
  and documented in the "Driver-internal locks" section above.
- Non-ranked by construction (owned_ops, WaitQueue, SHARED_MEMORY_REGISTRY).
