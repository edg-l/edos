# Lock-Order Invariant

Sibling document: [`drop-contract.md`](drop-contract.md).

> **Every thread that acquires two or more ranked locks must acquire them in
> strictly increasing rank order (lower rank first).**

Rank is an integer assigned per lock *class*, not per instance. Spacing is in
multiples of 10 so later insertions never force a renumbering.

Acquiring two instances of the *same* class (two inodes in `vfs::rename`, say) is
allowed only in a documented key-ordered pattern, through `ranked_lock_same!`.
The caller owns the key ordering; the rank system cannot detect a wrong-key-order
same-class deadlock.

Locks deliberately left outside the system are listed under
[Non-ranked locks](#non-ranked-locks) with the reasoning for each.

Rank constants live in `kernel/src/debug/lock_order.rs`. Violations panic at
acquisition time in debug builds, via a per-thread rank stack.

## Which lock primitive

Order is only half the problem: a spin lock is bounded only while its holder
keeps running, since every other CPU busy-waits behind it. Preemption is
involuntary, so a bare `spin::Mutex`/`spin::RwLock` guard can be held by a
descheduled thread.

| Primitive | Use for |
|---|---|
| `IrqSpinlock` | state an interrupt handler can reach; disables interrupts |
| `PreemptSpinlock` / `PreemptRwLock` | everything else shared between threads |
| `BlockingMutex` / `BlockingRwLock` | anything held across I/O or a park |

`Preempt*` suppresses preemption rather than interrupts, which is what a lock
held across real work needs: `memory_manager` walks page tables and `vmas` walks
the VMA tree, and charging those to interrupt latency would be far worse than
the problem being solved. Nothing reachable under a `Preempt*` guard may park;
`thread_park*`, `thread_sleep` and `thread_yield` debug-assert on it.

The scheduler's own locks (`rq`, `sleepers`, `SCHEDULERS`, `WaitQueue.inner`)
stay bare: they are taken by scheduler code in short, interrupt-disabled
sections, and wrapping them would recurse into the preemption counter.

---

## Rank table

| Rank | Lock | Type | Location |
|-----:|------|------|----------|
|  10 | `VFS` mount registry | `PreemptRwLock<BTreeMap>` | `fs/vfs.rs` |
|  30 | `inode.lock` (per-inode) | `BlockingRwLock<()>` | `fs/inode.rs` |
|  32 | `EfsDriver.alloc_mutex` | `BlockingMutex<()>` | `fs/efs/mod.rs` |
|  35 | `dentry_cache.inner` | `BlockingMutex<DentryCacheInner>` | `fs/dentry.rs` |
|  40 | `InodePages.pages` | `BlockingMutex<BTreeMap>` | `fs/page_cache.rs` |
|  42 | `InodePages.in_flight` | `IrqSpinlock<BTreeMap>` | `fs/page_cache.rs` |
|  50 | `InodePages.dirty_keys` | `BlockingMutex<Vec>` | `fs/page_cache.rs` |
|  60 | `inode.mappers` | `BlockingMutex<Vec<Weak<..>>>` | `fs/inode.rs` |
|  70 | `UserThread.vmas` | `Arc<PreemptSpinlock<VmaSet>>` | `thread/mod.rs` |
|  80 | `UserThread.memory_manager` | `Arc<PreemptSpinlock<MemoryManager>>` | `thread/mod.rs` |
| 100 | `DIRTY_INODES` | `IrqSpinlock<Vec<Weak<VfsInode>>>` | `fs/vfs.rs` |
| 110 | `BlockPageCache.shards[N]` | `BlockingMutex<ShardInner>` | `fs/block_page_cache.rs` |
| 120 | `BlockPageCache.journals` | `BlockingMutex<BTreeMap>` | `fs/block_page_cache.rs` |
| 130 | `Journal.checkpoint_tracker` | `BlockingMutex<BTreeMap>` | `fs/journal/mod.rs` |
| 140 | `CachedBlockPage.write_lock` | `BlockingMutex<()>` | `fs/block_page_cache.rs` |
| 150 | `Journal.state` | `BlockingMutex<JournalState>` | `fs/journal/mod.rs` |
| 160 | `EfsDriver.mutable` | `BlockingMutex<EfsMutableState>` | `fs/efs/mod.rs` |
| 170 | `AhciPort.legacy_lock` | `BlockingMutex<()>` | `drivers/ahci/port.rs` |
| 180 | `AhciPort.slot_waiters[i]` | `spin::Mutex<Option<Arc<AhciSlotOp>>>` | `drivers/ahci/port.rs` |
| 180 | `AhciPort.ncq_waiters[i]` | `spin::Mutex<Option<Arc<AhciNcqOp>>>` | `drivers/ahci/port.rs` |
| 190 | `AhciPort.mmio_lock` | `spin::Mutex<()>` | `drivers/ahci/port.rs` |
| 200 | `PCI_CONFIG_LOCK` | `spin::Mutex<()>` | `drivers/pci/config.rs` |
| 210 | `TTY_BUFFER` | `BlockingMutex<VecDeque<u8>>` | `drivers/tty.rs` |
| 220 | `Pipe` (per-pipe) | `Arc<BlockingMutex<Pipe>>` | `thread/pipe.rs` |
| 230 | `Pty` (per-pty) | `Arc<BlockingMutex<Pty>>` | `thread/pty.rs` |
| 900 | kernel-global mapper | `IrqSpinlock<MemoryManager>` | `memory/mapper.rs` |
| 910 | `FRAME_ALLOCATOR` | `IrqSpinlock<BitmapFrameAllocator>` | `memory/frame_allocator.rs` |

### Per-lock notes

**10, VFS mount registry.** Read-held across `fs.clone()`. No inner lock is taken
while held.

**30, `inode.lock`.** The outer per-inode gate, held across FS driver callbacks
(memfs, efs, fat32). Legal inner ranks: 32, 35, 40, 50, 60, 70, 80, and the deep
leaves 900 and 910.

**32, `EfsDriver.alloc_mutex`.** Serializes bitmap allocation (`alloc_inode`,
`alloc_block`) across CPUs, so `mutable` (160) can be released across block-cache
I/O without two allocators picking the same bit. Taken at the top of every EFS
alloc path, above every leaf the allocation transitively touches: BPC 110, journal
120 to 150, `mutable` 160, AHCI 170 to 200, kernel mapper 900, frame alloc 910.

**35, `dentry_cache.inner`.** Always acquired after `inode.lock`. Leaf on the
dentry side.

**40, `InodePages.pages`.** Inner: `dirty_keys` (50). Never held across disk I/O;
the fill paths drop it before calling `fill_fn`.

**42, `InodePages.in_flight`.** Per-inode registry of in-flight page fills,
installed by the publisher in `get_or_fill_async_sync` / `get_or_fill_bulk_async_sync`
and joined by slow-path readers. `IrqSpinlock` rather than `BlockingMutex` because
`PageFillHandle::cancel` runs on the reaper kthread and must not park; hold time is
one `get` plus one `remove`. The single nested edge is `pages (40) -> in_flight (42)`
during publisher remove. `fill_fn` runs with no `InodePages` lock held, so BPC (110),
journal (120 to 150), EFS `mutable` (160), AHCI (170 to 200) and the deep leaves
(900, 910) are all legally reachable from inside it. Reader state machine lives in
`fs/page_fill.rs`.

**50, `InodePages.dirty_keys`.** Leaf on the per-inode side.

**60, `inode.mappers`.** Inner: vmas (70) and per-process mm (80) on target
`UserThread`s during truncate.

**70, `UserThread.vmas`.** Per-process VMA set. Inner: per-process mm (80).

**80, `UserThread.memory_manager`.** Per-process page table, leaf for PTE walks.
Acquired alone or inside vmas (70). Distinct class from the rank-900 kernel mapper.

> **Caveat.** `copy_to_user`, `zero_user` and `write_val_to_user` go through
> `translate_to_hhdm_ptr`, whose demand-fault slow path acquires rank-70 vmas.
> A caller holding rank-80 mm MUST pre-map the target range eagerly (via
> `map_memory`) so the fast path is taken. See `memory/mapper.rs` and
> `allocate_tls_region` in `thread/thread.rs`.

**100, `DIRTY_INODES`.** Reached from `register_dirty_inode` (on the rank-30
`inode.lock` path) and from the writeback kthread (holding nothing). Takes no inner
lock.

**110, `BlockPageCache.shards[N]`.** Never held across disk I/O. Inner: `journals`
(120), `write_lock` (140). Sibling of the journal ranks 130 and 150, never co-held
with them.

**120, `BlockPageCache.journals`.** Brief; returns an `Arc<Journal>`. Leaf within
the block-cache subsystem.

**130 and 150, journal tracker and state.** Sibling leaves, never co-held in either
direction. See [Journal tracker and state](#journal-tracker-and-state).

**140, `CachedBlockPage.write_lock`.** Serializes partial writers on one page.
True leaf.

**160, `EfsDriver.mutable`.** Taken by EFS callbacks under `inode.lock` (30), and
under `alloc_mutex` (32) on alloc paths. A true leaf while held: every site releases
it before calling into the block page cache (110) or the journal (120, 130, 150).

**170, `AhciPort.legacy_lock`.** Serializes non-NCQ commands. Nested:
`slot_waiters` (180), `mmio_lock` (190).

**180, `slot_waiters[i]` and `ncq_waiters[i]`.** Brief per-slot state. Taken by
submit, the IRQ dispatcher, TFES `fail_all_ncq_slots` and the watchdog kthread.
Holders only do `Arc::clone` or `take`: they never park, never allocate, never take
an inner lock. Same rank, and never co-held with each other.

**190, `AhciPort.mmio_lock`.** Very short raw MMIO read-modify-write. True leaf.

**200, `PCI_CONFIG_LOCK`.** Config-space read-modify-write. Acquired alone.

**210, `TTY_BUFFER`. 220, `Pipe`. 230, `Pty`.** IPC and console endpoints, and
the only ranks reached from the syscall read/write path rather than from the FS
ladder. Two constraints fix where they sit. They must be **above 30**, because
`/dev/tty0` is a devfs device and devfs has no `PageCacheOps`, so a write to it
runs `TtyDevice::write` under `inode.lock` from `vfs::write_from_user`'s
non-page-cache branch. They must be **below 900**, because appending to any of
these buffers allocates and a heap expansion reaches the frame allocator.

Nothing ranked is acquired while one of them is held: the bodies do buffer
manipulation and, for the pty, line-discipline work. That is what makes them
safe to place anywhere in that window, and it is the property to re-check before
adding anything to those critical sections.

The three are never co-held with each other. Their relative order is therefore
arbitrary and only exists so the tracker has a total order.

**They are ranked primarily so `assert_no_guards_held` can see them.** A guard on
an unranked lock is invisible to the per-thread stack, so a thread dying while
holding one is undetectable. See "Guards and thread death" below.

**900, kernel-global mapper.** Reached via `memory_mapper()`. Kernel-address-space
edits plus per-page virtual-to-physical translation during DMA setup. A deep leaf,
called from arbitrary driver and FS contexts. Never co-held with a per-process
`memory_manager` (80). Inner: frame alloc (910) during `map_memory`.

**910, `FRAME_ALLOCATOR`.** Deep leaf, brief hold, no inner locks, called from
everywhere (BPC fills, page-table frame allocation, fault handlers). Ranked above
the kernel mapper so `map_memory` walks `900 -> 910`, which is ascending.

### Two mappers, two ranks

Rank 80 and rank 900 are both `MemoryManager`, and they are deliberately different
classes because they govern different address spaces:

| | Rank 80 | Rank 900 |
|---|---|---|
| Object | `UserThread.memory_manager` | global, via `memory_mapper()` |
| Type | `Arc<PreemptSpinlock<MemoryManager>>` | `IrqSpinlock<MemoryManager>` |
| Governs | user address space | kernel address space (kmap, device memory) |
| Acquired | inside vmas (70) | alone, or on frame-allocator paths |

They are never co-held. Separate ranks mean an accidental co-acquisition panics in
debug builds instead of deadlocking in production.

### Journal tracker and state

`checkpoint_tracker` (130) and `state` (150) are sibling leaves: no path co-holds
them, in either order.

This is load-bearing, because they used to invert. `is_safe_to_flush` and
`advance_tail` held the tracker while calling `committed_seq()`, which takes
`state` internally, giving `130 -> 150`. `TxHandle::drop` took the same two in the
opposite order. Classic AB/BA. The fix hoists `committed_seq()` above the tracker
acquisition at both sites. Any future caller that co-holds them will trip the rank
tracker.

---

## Non-ranked locks

### `Thread.owned_ops`

`IrqSpinlock<heapless::Vec<..>>` in `thread/thread.rs`.

Acquired from unrelated call sites: AHCI submission (sometimes under FS locks),
completion, and the thread reaper inside `Thread::free`. Any fixed rank produces
false positives; too low fires when acquired under FS locks, too high fires when
acquired bare.

It is a leaf by construction. Every site is `lock() -> mutate Vec -> drop()`, with
no inner lock and no park inside the scope:

| Site | Context |
|---|---|
| `with_slot_blocking` | before park, no ranked lock held |
| `with_slot_try` | before submission, no ranked lock held |
| batch read loop | before `wait_for_ncq_completion`, no ranked lock held |
| remove-after-wake sites | after park returns, no ranked lock held |
| `owned_ops_cancel_all` | reaper context; drains under lock, cancels outside |

### `WaitQueue.inner`

`spin::Mutex<Deque<Weak<Thread>>>` in `thread/waitqueue.rs`.

Held only for `push_back` / `pop_front` inside `without_interrupts` micro-sections;
the park itself happens with the lock released. Ranking it would fire on every
`wait_until` call, which is noise. The real hazard is already covered by the
`debug_assert!` in `BlockingMutex::lock`, which rejects contended acquisition with
interrupts disabled.

### `BlockingMutex.waiters`

Uses `WaitQueue` internally; same reasoning.

### Scheduler `rq`, `sleepers`, `SCHEDULERS`

Internal to park/wake plumbing, never held across user code, never a multi-lock
participant. Violations surface as deadlocks caught by the existing park/wake
assertions.

### `SHARED_MEMORY_REGISTRY`

`PreemptRwLock<BTreeMap>` in `memory/shared.rs`. Never co-held with vmas (70) or mm
(80), because `syscalls/shm.rs` always drops the registry guard first:

- `SharedMemory::get()` takes a read guard and immediately clones the `Arc`,
  dropping the guard before any vmas or mm acquisition.
- `SharedMemory::dec_ref()` may take a write guard when the refcount hits zero, but
  only after `vmas.lock()` has been dropped; the VMA is removed first, then
  `dec_ref` runs on the returned value.
- `SharedMemory::destroy()` takes a write guard with nothing else held.

### `WINDOW_REGISTRY` and `WINDOW_EVENTS`

Both `PreemptRwLock`, in `window/registry.rs` and `window/input.rs`. Audited
2026-08-08 after a hang in which all four CPUs spun on `WINDOW_REGISTRY.write()`
(see `doc/bugs/2026-08-08-window-registry-stuck-reader.md`).

`PreemptRwLock` suppresses preemption for the guard's lifetime, so a critical
section cannot be descheduled and other CPUs never spin for longer than the
section itself.

The order is **`WINDOW_REGISTRY` before `WINDOW_EVENTS`**, and it holds
everywhere:

- The only nesting is `send_event`, which takes `WINDOW_EVENTS.read()` while a
  `WINDOW_REGISTRY` read guard is live. It does a lock-free `ArrayQueue` push:
  no allocation, no park.
- `poll_events` pre-allocates its `Vec` before taking `WINDOW_EVENTS.read()`,
  and its caller drops the registry guard first.
- The `WINDOW_EVENTS` write paths (`get_or_create_event_queue`,
  `remove_event_queue`) are called only after every registry guard has been
  dropped, at `syscalls/window.rs:46`, `:83` and `window/mod.rs:32`.

Because these are spin locks, hold *duration* matters more than order: a holder
that stops making progress stops every other CPU dead rather than just one
caller. Nothing reachable under either guard may park or touch user memory.
That rule was violated by `sys_window_list`, which held the read guard across
`try_copy_to_user`, and is why the window list is now snapshotted before the
copy.

A holder does not have to park to stall: preemption is involuntary, so any
guard can be held by a `Ready` thread. What bounds that wait is the scheduler
refusing to starve a runnable thread (`RunQueue::pop_next` services a lower
level every `STARVE_STREAK_LIMIT` picks, and `Scheduler::expire_timeslice`
ends a slice that has elapsed). Without both, a spin lock shared by threads at
different priorities deadlocks outright: see
`doc/bugs/2026-08-08-window-registry-stuck-reader.md`.

Ranking them is now possible, since the order is established; it would not have
caught the hang, which was a hold-duration failure rather than an inversion.

Allocation still happens under both guards (`sys_window_list` builds its
snapshot, `create_window` inserts into a `BTreeMap`), and therefore with
interrupts disabled. That is an established pattern here — the allocator's own
fast path runs inside `without_interrupts` and every level of it is an
`IrqSpinlock` — and the amount allocated is bounded by the window count. A heap
expansion landing inside one of these sections would be a long interrupts-off
window; if that ever shows up as latency, reserve outside the guard.

### `FUTEX_REGISTRY`, `PORT_TABLE`

Leaf locks outside the FS and MM hot paths. Rank them if a real ordering concern
ever surfaces.

---

## Driver-internal locks

These are internal to FS driver implementations, called from `FileSystem` trait
callbacks that always run under `inode.lock` (30). They are not in multi-lock paths
with each other, so they are unranked.

| Lock | Driver | Notes |
|---|---|---|
| `Fatfs.write_lock` | fat32 | serializes fat32 mutations |
| `Fatfs.inode_table` | fat32 | per-instance side table |
| `Fatfs.fs_info` | fat32 | FS info sector cache |
| `DevFs.shared` | devfs | device registry |
| `MemFs.inner` | memfs | node tree |

---

## Guards and thread death

Rank order stops two threads deadlocking against each other. It says nothing
about a thread that stops running while holding a guard, which is a separate way
to lose a lock forever.

There is no unwinding in this kernel. A thread killed while holding a guard never
runs that guard's `Drop`, so the lock is never released and every later acquirer
blocks for good. This is the sibling of the drop contract: that one says a `Drop`
must not block, this one says **a guard must not be live where a thread can die.**

`lock_order::assert_no_guards_held`, called at the top of `thread_exit`, is the
enforcement. Every path that ends a thread funnels through there. It sees ranked
locks only, which is the practical reason to rank a lock that is otherwise a
leaf: an unranked guard is invisible to it.

The places a thread can actually die are fewer than they look, and knowing which
bounds any audit of this:

| Kill point | Can a guard be live? |
|---|---|
| GPF, invalid opcode, alignment check, page fault (ring 3) | No — interrupted user code holds nothing |
| Timer tick (`tick_prepare`, ring-3 frame only) | No — same reason; the ring-3 check *is* the proof |
| `exit_if_killed` at the syscall return boundary | No — the body returned and dropped its guards |
| Page fault, ring 0, inside a `try_copy_*` | No — takes the uaccess fixup and returns EFAULT |
| An explicit `thread_exit()` inside a syscall body | **Yes.** Two exist; both are currently outside their guards |

The subtle one is the user copy. In the ring-0 branch of `page_fault_handler`,
`handle_demand_fault` runs *before* the uaccess fixup so a copy touching a
lazily-mapped page gets it mapped rather than failing — and that handler blocks
on NCQ I/O, block-page-cache contention and vma waitqueues, with interrupts
re-enabled. So a guard live across `try_copy_to_user` / `try_copy_from_user`
spans a park. Buffer first, lock second: copy out of user space before taking the
lock, and drain into an owned buffer under the lock and copy out after dropping
it.

## Adding a lock

1. **Pick a rank.** Use a multiple of 10 in the right range. To slot between two
   existing ranks, use an intermediate value (rank 42 sits between 40 and 50). Keep
   the 10-unit spacing; if you run out of room, drop to 5 and say why.
2. **Add it to the table**, with type, file, and a note on which inner locks may be
   taken while it is held.
3. **Wrap every acquisition site.** `ranked_lock!(rank, "name", lock)`,
   `ranked_read!`, or `ranked_write!`. For `IrqSpinlock` use
   `lock_ranked(rank, "name")`; for `BlockingRwLock` use `read_ranked` /
   `write_ranked`.
4. **Say so if it is a leaf** (no inner ranked locks).
5. **Document the key ordering** if it takes part in a same-class two-instance
   pattern like `inode.lock` in `vfs::rename`, and use `ranked_lock_same!` there.
