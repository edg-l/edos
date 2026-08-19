# `sync` that returned before a file's extents were committed

## Status

Fixed. `sys_sync`'s fixed point is `Journal::needs_sync_round`, which counts the
open transaction.

## Symptoms

`edos-install` reported success, the guest was stopped the instant it exited,
and the installed disk did not boot:

```
efs journal: scanned, nothing to replay
Root filesystem mounted
efs: read hole at logical block 1 (byte 4096) of a 248448-byte file, 1 extents mapped
Spawned bin/edos-init tid=20 cpu=0
```

`bin/edos-init` has its correct on-disk size and exactly one mapped extent, so
everything past its first 4 KiB reads as zeros. The kernel spawns the truncated
binary and the desktop never comes up. Leaving the same guest alive for one
5 s writeback period before stopping it produces a disk that boots, which is
what made this look like a timing accident rather than a defect.

## Root cause

A file's size and a file's extents reach the disk by different routes.
`page_cache_write` stamps the size **synchronously** through
`pc_ops.update_size`, which is why `vfs::flush_dirty_inodes` passes `None` for
it. The extents are allocated much later, by the flush pass that writes the
dirty pages, and `flush_page` / `flush_pages_bulk` enrol the inode and bitmap
blocks carrying them into the journal's **active** transaction — `TxHandle::drop`
merges into `state.active`; it does not seal.

`sys_sync` loops commit-then-flush to a fixed point, and its fixed point was
`needs_checkpoint()`: `sealed` non-empty or `committed_pending` non-empty. That
is blind to the active transaction, so the very round that flushed the data and
created the metadata read as converged:

1. commit the active transaction (whatever predated the sync),
2. flush — writes every dirty page, allocates their extents, enrols them in the
   new active transaction,
3. `advance_tail` retires what step 1 committed,
4. nothing sealed, nothing pending → converged, break.

`sync` then returned with every extent the flush had just allocated in memory
only. A prompt reboot lost all of them, and the one extent that survived per
file is the block `convert_inline_to_extents` allocated at first write, back
when `update_size` had to leave inline mode — committed by the periodic
committer long before the sync.

The 5 s grace period worked because the *next* writeback pass enrols the same
metadata again and the committer kthread commits it on its own schedule.

## The reasoning that produced it

`doc/bugs/2026-08-09-sync-that-left-the-journal-dirty.md` states as a
termination requirement that the loop "tests only committed work", because "the
open transaction is refilled by every flush and is never replayed, so a
condition that counts it never goes false". Never-replayed is true and is
exactly the problem: an open transaction is not replayed, so leaving it open is
losing it. That post-mortem was asking whether `sync` leaves the journal
*replay-clean*; the question that matters to a caller is whether `sync` leaves
the caller's writes *durable*, and the two differ by precisely this
transaction.

Termination survives the stricter test because each round commits the active
transaction before flushing again: round N's enrolments are round N+1's
committed work, and a pass that writes nothing new enrols nothing. Convergence
normally takes two rounds.

## How to catch a recurrence

`make nvme-check` case 4 installs onto a blank NVMe disk, stops the guest the
moment `edos-install` exits, and boots the installed disk. Do not add a settle
there: a user who reboots promptly must get the same disk the gate does. From a
guest, `/proc/journal_stats` now reports `active` beside `sealed`, `pending` and
`tracked`, so a `sync` that returns with metadata uncommitted can be read rather
than inferred; `sys_sync: journal still pending after 8 rounds` in `run_log.txt`
means the loop hit its cap.
