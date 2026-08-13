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
|  32 | `EfsDriver.bitmap_mutex` | `BlockingMutex<()>` | `fs/efs/mod.rs` |
|  33 | `EfsDriver.orphan_prev` | `BlockingMutex<BTreeMap<u64,u64>>` | `fs/efs/mod.rs` |
|  35 | `dentry_cache.inner` | `BlockingMutex<DentryCacheInner>` | `fs/dentry.rs` |
|  36 | `INODE_CACHE` | `BlockingMutex<BTreeMap<(usize,u64), Weak<VfsInode>>>` | `fs/icache.rs` |
|  40 | `InodePages.pages` | `BlockingMutex<BTreeMap>` | `fs/page_cache.rs` |
|  42 | `InodePages.in_flight` | `IrqSpinlock<BTreeMap>` | `fs/page_cache.rs` |
|  50 | `InodePages.dirty_keys` | `BlockingMutex<Vec>` | `fs/page_cache.rs` |
|  60 | `inode.mappers` | `BlockingMutex<Vec<Weak<..>>>` | `fs/inode.rs` |
|  70 | `UserThread.vmas` | `Arc<PreemptSpinlock<VmaSet>>` | `thread/mod.rs` |
|  80 | `UserThread.memory_manager` | `Arc<PreemptSpinlock<MemoryManager>>` | `thread/mod.rs` |
|  90 | `SHARED_MEMORY_REGISTRY` | `PreemptRwLock<BTreeMap>` | `memory/shared.rs` |
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
| 204 | `Mailbox.queue` | `BlockingMutex<VecDeque>` | `thread/mailbox.rs` |
| 206 | `ResponseInner.value` | `BlockingMutex<Option<R>>` | `thread/mailbox.rs` |
| 210 | `TTY_BUFFER` | `BlockingMutex<VecDeque<u8>>` | `drivers/tty.rs` |
| 215 | `FIFO_REGISTRY` | `PreemptSpinlock<BTreeMap<FifoKey, Arc<Fifo>>>` | `fs/fifo.rs` |
| 220 | `Pipe` (per-pipe) | `Arc<BlockingMutex<Pipe>>` | `thread/pipe.rs` |
| 230 | `Pty` (per-pty) | `Arc<BlockingMutex<Pty>>` | `thread/pty.rs` |
| 240 | `NET_STACK` | `PreemptSpinlock<NetStack>` | `net/stack.rs` |
| 250 | `PORT_TABLE` | `PreemptSpinlock<BTreeMap>` | `net/socket.rs` |
| 260 | `Socket` (per-socket) | `Arc<PreemptSpinlock<Socket>>` | `net/socket.rs` |
| 270 | `TcpConnection` (per-conn) | `Arc<PreemptSpinlock<TcpConnection>>` | `net/tcp.rs` |
| 275 | `SHELL_PIDS` | `PreemptRwLock<Vec<u64>>` | `window/shell.rs` |
| 280 | `WINDOW_REGISTRY` | `PreemptRwLock<WindowRegistry>` | `window/registry.rs` |
| 290 | `WINDOW_EVENTS` | `PreemptRwLock<BTreeMap>` | `window/input.rs` |
| 295 | `BUFFERS` (clipboard) | `PreemptSpinlock<Buffers>` | `window/clipboard.rs` |
| 300 | `LAST_MOUSE_BUTTONS` | `PreemptSpinlock<u8>` | `window/input.rs` |
| 310 | `Broadcaster.subs` | `PreemptRwLock<BTreeMap>` | `thread/broadcast.rs` |
| 320 | device poller lists | `BlockingMutex<Vec>` | `drivers/{mouse,keyboard}/mod.rs`, `drivers/tty.rs` |
| 330 | `HdaPlaybackState` | `Arc<PreemptSpinlock<..>>` | `drivers/hda/mod.rs` |
| 340 | `DevFs.shared` | `Arc<PreemptRwLock<DevFs>>` | `fs/devfs/mod.rs` |
| 350 | `EVICT_OVERFLOW` | `PreemptSpinlock<Vec<EvictRequest>>` | `fs/evict.rs` |
| 355 | trace ring | `PreemptSpinlock<Option<Ring>>` | `syscalls/trace.rs` |
| 900 | kernel-global mapper | `IrqSpinlock<MemoryManager>` | `memory/mapper.rs` |
| 910 | `FRAME_ALLOCATOR` | `IrqSpinlock<BitmapFrameAllocator>` | `memory/frame_allocator.rs` |

### Per-lock notes

**10, VFS mount registry.** Read-held across `fs.clone()`. No inner lock is taken
while held.

**30, `inode.lock`.** The outer per-inode gate, held across FS driver callbacks
(memfs, efs, fat32). Legal inner ranks: 32, 35, 40, 50, 60, 70, 80, and the deep
leaves 900 and 910.

**31, `EfsDriver.inode_rmw`.** An EFS inode is 256 bytes written as a unit, and
several paths read one, change a single field and write the whole struct back:
`update_size` stamps size, `ensure_block*_for_logical` appends extents,
`truncate_inner` trims them. Unserialized, the size write puts back the extent
list as it was before the append, and the blocks that append allocated stay set
in the block bitmap with nothing referencing them --- a steady leak proportional
to write volume, with no error reported anywhere. Held across block-cache I/O,
so it is a `BlockingMutex`; entry points take it once and call the `_locked`
inner variants, since it is not reentrant.

**33, `EfsDriver.orphan_prev`.** In-memory mirror of the on-disk orphan chain
(`doc/efs.md` §14), mapping an inode to the one that points at it so an eviction
can unlink it without walking the chain from the head. It is also the chain's
lock: `orphan_add` and `orphan_del` hold it across the superblock and inode writes
that change the links, which is what serializes two concurrent unlinks against one
head. Those writes reach the block page cache (110) and take `mutable` (160), so it
has to sit below both. It is never co-held with `bitmap_mutex` (32): the chain is
updated before storage is freed, and freeing is what takes the bitmap.

**32, `EfsDriver.bitmap_mutex`.** Guards *every* read-modify-write of an
allocation bitmap, not just allocation. `alloc_block`, `free_block`,
`alloc_inode` and `free_inode` all read a bitmap block, flip one bit, and write
the whole block back, and `mutable` is deliberately released across that I/O.
Serializing only the allocators — which is what this lock originally did — lets
a concurrent free read the same block, clear its own bit, and write back a copy
that still has the allocator's bit clear, or vice versa. Either way one bit is
lost: a freed block that stays marked (leaked space) or an allocated block that
reads free (handed out twice). Taken above `inode_rmw` (31), which several
callers already hold when they allocate or free.
**35, `dentry_cache.inner`.** Always acquired after `inode.lock`. Leaf on the
dentry side.

**36, `INODE_CACHE`.** Maps `(mount_id, ino)` to a `Weak<VfsInode>` so a path
whose dentry entry was invalidated resolves back to the inode that already owns
the file's page cache. Leaf, and taken only by `resolve_inode_for` once the
dentry lookup has missed and released its lock — above 35 because a dentry
invalidation drops its `Arc<VfsInode>` with the dentry lock held.

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

**90, `SHARED_MEMORY_REGISTRY`.** Inside *both* address-space locks. Two paths
co-hold it with them, which is why the earlier "never co-held with vmas (70) or
mm (80)" reading was wrong — it audited `syscalls/shm.rs` alone and missed the
two callers outside that file:

- `sys_fork`'s deep copy resolves each `SharedMemory` VMA's region
  (`SharedMemory::get`, then `inc_ref`) with the rank-70 vmas guard live.
- `release_mappings` does the same under the rank-80 page-table guard: it
  unmaps the region's pages and drops the process's reference in one pass, and
  `Thread::free` holds that guard across the whole call.

The shm syscalls themselves take it alone. `get` clones the `Arc` and drops the
guard before any address-space work, `destroy` takes a write guard with nothing
else held, and `dec_ref`'s write guard is entered only after the vmas guard has
gone. Its own inner reach is the frame allocator (910), when the last `Arc`
drops under the write guard in `dec_ref`.

**100, `DIRTY_INODES`.** Reached from `register_dirty_inode` (on the rank-30
`inode.lock` path) and from the writeback kthread (holding nothing). Takes no inner
lock.

**110, `BlockPageCache.shards[N]`.** Never held across disk I/O. Inner: `journals`
(120), `write_lock` (140). Sibling of the journal ranks 130 and 150, never co-held
with them.

`invalidate_pages` is called by the filesystems right after they write file data
straight to the device, so it runs on a path that has already released
`EfsDriver.mutable` (160). Taking 110 there is only legal because of that: the
evicted pages are dropped outside the shard lock, since `CachedBlockPage::drop`
frees a frame (910).

**120, `BlockPageCache.journals`.** Brief; returns an `Arc<Journal>`. Leaf within
the block-cache subsystem.

**130 and 150, journal tracker and state.** Sibling leaves, never co-held in either
direction. See [Journal tracker and state](#journal-tracker-and-state).

**140, `CachedBlockPage.write_lock`.** Serializes partial writers on one page.
True leaf. Writeback holds one per outstanding write while a batch of dirty
pages is in flight, so it is also a same-class multi-acquire: `write_batch` in
`fs/block_page_cache.rs` sorts its batch by `(device_id, page_block_idx)` and
takes the guards through `ranked_lock_same!` in that order. Its batch cap comes
from `LOCK_RANK_DEPTH`, since each outstanding write costs a rank-stack slot.

**160, `EfsDriver.mutable`.** Taken by EFS callbacks under `inode.lock` (30), and
under `bitmap_mutex` (32) on alloc and free paths. A true leaf while held: every site releases
it before calling into the block page cache (110) or the journal (120, 130, 150).

**170, `AhciPort.legacy_lock`.** Serializes non-NCQ commands. Nested:
`slot_waiters` (180), `mmio_lock` (190).

**180, `slot_waiters[i]` and `ncq_waiters[i]`.** Brief per-slot state. Taken by
submit, the IRQ dispatcher, TFES `fail_all_ncq_slots` and the watchdog kthread.
Holders only do `Arc::clone` or `take`: they never park, never allocate, never take
an inner lock. Same rank, and never co-held with each other.

**190, `AhciPort.mmio_lock`.** Very short raw MMIO read-modify-write. True leaf.

**200, `PCI_CONFIG_LOCK`.** Config-space read-modify-write. Acquired alone.

**204, `Mailbox.queue`. 206, `ResponseInner.value`.** The request/response
transport between a caller and a driver kthread: `USB_BLOCK_MAILBOX` carries
USB mass-storage reads and writes, `FS_REQUESTS` carries mounts and partition
registration. Ranked as one class each rather than per instance, which is sound
because no path holds one mailbox's queue guard while taking another's: `send`,
`recv`, `try_recv` and `forward` all scope the guard to a single push or pop.

They sit just above the AHCI band because a USB block request is issued from the
same FS depth an AHCI command is — under `inode.lock` (30) or `EfsDriver`'s
`bitmap_mutex` (32) — and the two device stacks are never co-held. The 2-unit
spacing is because the driver band below and the console band above leave no
10-unit gap; both are leaves, so nothing will need to slot between them.

`Mailbox::is_empty` stays outside the system: it is a `try_lock` probe, called
with interrupts disabled from the xHCI dispatcher, and the macros have no
try_lock form.

**210, `TTY_BUFFER`. 215, `FIFO_REGISTRY`. 220, `Pipe`. 230, `Pty`.** IPC and console endpoints, and
the only ranks reached from the syscall read/write path rather than from the FS
ladder. Two constraints fix where they sit. They must be **above 30**, because
`/dev/tty0` is a devfs device and devfs has no `PageCacheOps`, so a write to it
runs `TtyDevice::write` under `inode.lock` from `vfs::write_from_user`'s
non-page-cache branch. They must be **below 900**, because appending to any of
these buffers allocates and a heap expansion reaches the frame allocator.

`FIFO_REGISTRY` is the one pair in this band that is genuinely co-held, and in
that order: `fifo::incarnation` holds the registry while it takes a `Pipe` to
ask whether that named pipe still has an end open. Never the other way round --
nothing holding a pipe looks a FIFO up -- which is what the 5-unit gap below 220
records. The console band below it leaves no 10-unit room.

Nothing else ranked is acquired while one of them is held: the bodies do buffer
manipulation and, for the pty, line-discipline work. That is what makes them
safe to place anywhere in that window, and it is the property to re-check before
adding anything to those critical sections.

The three are never co-held with each other. Their relative order is therefore
arbitrary and only exists so the tracker has a total order.

**They are ranked primarily so `assert_no_guards_held` can see them.** A guard on
an unranked lock is invisible to the per-thread stack, so a thread dying while
holding one is undetectable. See "Guards and thread death" below.

**240 to 270, networking.** The order is fixed by the receive path, not chosen:
`handle_udp`/`handle_tcp` run as `&mut self` on the stack (so `NET_STACK` is
already held), take the port table to find the socket, and a socket's
`poll_state` locks its connection. That gives
`NET_STACK -> PORT_TABLE -> SOCKET -> TCP_CONN`.

Ranking them turned up two pre-existing AB/BA inversions, both of the same
shape — taking the port table while holding something that belongs inside it:

- **`tcp_retransmit_main`'s cleanup** took `port_table` inside the `retain`
  closure, while the connection guard was live, to free the ephemeral port.
  That is `TCP_CONN -> PORT_TABLE`, closing the cycle
  `PORT_TABLE -> SOCKET -> TCP_CONN -> PORT_TABLE`. It survived only because
  the socket held under the port table in `handle_tcp` is always a *listening*
  one, whose `poll_state` reads the accept queue instead of a connection.
  Nothing enforced that. Fixed by collecting the freed ports under the guard
  and removing them after it.
- **`close_descriptor`'s socket arm** took `port_table` while the socket guard
  was live, which is `SOCKET -> PORT_TABLE` against the receive path's
  `PORT_TABLE -> SOCKET`. Closing a listening socket while a segment arrived
  for it would have wedged two CPUs on preempt spinlocks. Fixed by reading the
  key under the guard and removing after the existing `drop(s)`.

Both are the reason the rank system pays for itself here: neither is visible by
reading either function alone, and both sit between a syscall and a driver
kthread that genuinely run at the same time.

**275, `SHELL_PIDS`.** Which processes may manage windows they do not own.
Strictly outside `WINDOW_REGISTRY`: a window syscall settles the caller's
authority *before* it touches the registry, because the answer does not depend
on the window, and because co-holding them would be two locks of one rank,
which the tracker rejects. It caught exactly that the first time this was
written.

**280 to 300, window system.** The input thread holds the registry across event
delivery (`handle_keyboard_event` sends key events while its read guard is
live), and nothing on the event-queue side — `send_event`, `poll_events`,
`get_or_create_event_queue`, `remove_event_queue` — reaches back into the
registry. So the registry is strictly outside, and `WINDOW_EVENTS` is a leaf
relative to it. `LAST_MOUSE_BUTTONS` is taken under the registry read in
`handle_mouse_event` and explicitly dropped before anything else, so it is never
co-held with `WINDOW_EVENTS`; its rank exists only to give the tracker a total
order.

**295, the clipboard.** A leaf of the same band, and the strictest case of the
same pattern: `sys_clipboard_get` and `sys_clipboard_set` hold nothing when they
take it and reach nothing under it. Both copy to and from user memory outside
the guard, since `try_copy_to_user` can demand-fault and a demand fault can park
on a page fill, which is the rule the window list already follows for the same
reason. Its rank buys guard visibility in `assert_no_guards_held` rather than
deadlock freedom.

No inversions were found here, unlike the FS and networking sweeps. The two
sites that look like violations are already correct on purpose:
`handle_mouse_event` drops its read guard before upgrading to a write lock on a
focus change, and `cleanup_process_windows` scopes its read guard in a block.

The registry is only ever read through `read_tracked`, so the rank enter/exit
lives inside that function and its guard's `Drop` — one call covers every read
site in the kernel. Write sites use `ranked_write!` individually.

**310, `Broadcaster.subs`. 320, device poller lists.** Input delivery state,
written by the PS/2 keyboard and mouse kthreads and by the xHCI driver thread,
read by the window input thread and by poll callers. Rank 320 covers
`MOUSE_POLLERS`, `KEYBOARD_POLLERS` and `TTY_POLLERS` as one class: three
instances of the same "list of `PollEntry`s to update" pattern, never co-held
with each other. They are ranked above the window band because the window input
thread is the one context that could hold a registry guard while touching them;
the driver kthreads hold nothing at all when they broadcast. `Broadcaster.subs`
is never co-held with a poller list: `MousePoll::register` calls `subscribe` and
lets that guard go before taking the list.

`TTY_BUFFER` (210) *is* co-held with `TTY_POLLERS` (320), in increasing order,
and deliberately. Both `tty::snapshot_pollers` and `TtyPoll::register` take the
list while still holding the buffer, because that is what serializes them: a
registration that read an empty buffer and then joined the list without the
buffer lock would miss the wake for the bytes that arrived in between. What
must not happen under either lock is the wake itself, so the snapshot only
clones the entries and the caller calls `TtyNotifications::flush` once both are
dropped. Pipes, PTYs and sockets defer their notifications the same way.

`Broadcaster.subs` was a bare `spin::RwLock` and is now a `PreemptRwLock`. It is
shared between threads, so a descheduled holder used to stall every other CPU
behind it — the shape of the 2026-08-08 window-registry hang. `subscribe` also
built its 256-slot `ArrayQueue` under the write guard; the allocation now
happens before the lock is taken.

**330, `HdaPlaybackState`.** The whole audio driver behind one lock: the
controller registers, the BDL ring cursors and the stream-running flag. Held by
`/dev/dsp` writers (under `inode.lock`, 30) and by the HDA kthread when a BCIS
interrupt advances the read cursor. It was a bare `spin::Mutex` — held across a
memcpy loop into the DMA ring, so a descheduled writer stalled the audio IRQ
thread — and is a `PreemptSpinlock` now. `AUDIO_IOCTL_DRAIN` polls in a loop and
drops the guard before each `thread_yield`, which the park assertions enforce.

**350, `EVICT_OVERFLOW`.** Holds orphan-eviction requests the reaper could not
fit in the 256-entry ring. Reachable from any `VfsInode::drop`, including the
reaper's, so it outranks everything such a drop could still hold. Both sides
take it, move the `Vec`, and release: the evict kthread must not hold it across
`evict_inode`, which does disk I/O and takes EFS locks far below this rank.

**340, `DevFs.shared`.** The devfs device registry, deliberately outermost: it
must be released before dispatching into a `DevFsDevice`, and ranking it above
every device lock is what turns "forgot to drop the guard" into a panic instead
of a spin lock held across a driver callback.

That is not hypothetical. Ranking it caught exactly that on the first `ls /dev`:

```
lock order violation: tried to acquire 'tty::device_size' (rank 210)
while holding 'devfs::list_files' (rank 340);
full stack: [inode.lock(30), devfs::list_files(340)]
```

`read_bytes`, `write_bytes`, `ioctl`, `poll` and `mmap` all drop the guard
before calling the device. `list_files` and `file_info` did not, because their
call into the driver does not look like a dispatch: `DeviceNode::file_entry`
reads `DevFsDevice::size`, and for `/dev/tty0` that takes the rank-210
`BlockingMutex`. A spin lock held across a *blocking* mutex acquisition is the
serious half — the holder can park with the registry still locked. Both now
snapshot the nodes under the guard and build their `File` entries after it.

**355, trace ring.** The syscall tracer's record ring. A true leaf: it is taken
at the syscall entry and return boundaries, where no other guard is live, and in
`thread_exit`, which has just asserted the same thing. Nothing is acquired while
it is held and it is never held across a call into anything else, so its rank
would be legal anywhere. It is ranked so that a thread dying with it held is
caught by `assert_no_guards_held` rather than leaving the ring wedged for every
other CPU; an unranked lock is invisible to that check.

The ~250 KiB `Vec` behind it is dropped *outside* the guard, since freeing it
reaches the allocator.

**900, kernel-global mapper.** Reached via `memory_mapper()`. Kernel-address-space
edits plus per-page virtual-to-physical translation during DMA setup. A deep leaf,
called from arbitrary driver and FS contexts. Never co-held with a per-process
`memory_manager` (80). Inner: frame alloc (910) during `map_memory`.

**910, `FRAME_ALLOCATOR`.** Deep leaf, brief hold, no inner locks, called from
everywhere (BPC fills, page-table frame allocation, fault handlers). Ranked above
the kernel mapper so `map_memory` walks `900 -> 910`, which is ascending.

### Window locks: hold duration, not just order

Ranks 280 and 290, both `PreemptRwLock`, in `window/registry.rs` and
`window/input.rs`. Audited 2026-08-08 after a hang in which all four CPUs spun
on `WINDOW_REGISTRY.write()` (see
`doc/bugs/2026-08-08-window-registry-stuck-reader.md`). The ranks came later and
would not have caught that hang: it was a hold-duration failure, not an
inversion. This section is why the two are worth reading about beyond their row
in the table.

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

Allocation still happens under both guards (`sys_window_list` builds its
snapshot, `create_window` inserts into a `BTreeMap`), and therefore with
interrupts disabled. That is an established pattern here — the allocator's own
fast path runs inside `without_interrupts` and every level of it is an
`IrqSpinlock` — and the amount allocated is bounded by the window count. A heap
expansion landing inside one of these sections would be a long interrupts-off
window; if that ever shows up as latency, reserve outside the guard.

### Every subsystem on the list is ranked now

`ideas.txt`'s extension list is finished: FS/MM, AHCI, IPC and console,
networking, the window system, USB, the input path, shared memory, audio and
devfs. What is left outside the system is the
[Non-ranked locks](#non-ranked-locks) section, and every entry there has a
reason rather than a backlog position.

### The USB stack owns no locks

Ranking USB (2026-08-10) turned up nothing to rank inside it, and that is the
finding rather than a gap in the sweep. `XhciController` is reached only as
`&mut self` from `xhci_driver_main`; the MSI-X handler wakes that thread and
touches no controller state, and command rings, the device slot table and HID
report state are all local to it. Every other thread reaches the controller
through a channel instead of a lock:

| Edge | Mechanism | Rank |
|---|---|---|
| block I/O in, replies out | `USB_BLOCK_MAILBOX` | 204 / 206 |
| HID key and mouse events out | `KEY_EVENT_BROADCAST`, `MOUSE_BROADCAST` | 310 |
| `/dev/kbd`, `/dev/mouse` poll wakeups | poller lists | 320 |

Those are the ranks the sweep added, and none of them is USB-specific: the same
mailbox carries FS mount requests and the same broadcasters carry PS/2 input.

One real defect came out of it, on the delivery side rather than the locking
side. The USB HID paths broadcast to subscribers but never updated the poll
entries, which the PS/2 paths did — so with a USB keyboard or mouse attached
(the default under `make run`, and `USB_*_ACTIVE` suppresses the PS/2 producer),
`poll()` on `/dev/kbd` or `/dev/mouse` never reported readable. Both halves now
sit behind `dispatch_key_events` / `dispatch_mouse_event`, so a future producer
cannot do one without the other.

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

### `FUTEX_REGISTRY`

Leaf lock outside the FS and MM hot paths. Rank it if a real ordering concern
ever surfaces. `PORT_TABLE` used to be listed here and is now rank 250.

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
