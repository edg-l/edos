# Journal replay read the ring from outside the partition

## Status

Fixed. Three commits' worth of change in `kernel/src/fs/journal/replay.rs`:

- the ring read now adds `partition_start_lba`, which is the actual bug;
- pass 1 stops at `head_block` instead of scanning until a header fails to parse;
- the home-block writes are queued rather than one round trip each.

`kernel/src/fs/efs/mod.rs` passes `jsb_head_block` through for the second.

## Symptoms

None, which is the point. Every unclean mount logged

```
efs journal: replay start tail_seq=N head_seq=M
efs journal: no committed transactions to replay
```

and carried on. The journal existed, transactions were being committed to it
correctly, and recovery applied **nothing**. Metadata the journal was holding for
an interrupted mount was discarded rather than replayed.

It survived this long because a clean shutdown leaves nothing to replay and the
usual mount takes the `clean, no replay needed` path, so the broken branch almost
never ran. Reproducing it took rewinding `tail_seq`/`tail_block` in the journal
superblock on the host, because no userspace workload reliably leaves committed,
un-checkpointed transactions behind: writeback checkpoints promptly.

## Root cause

`replay()` receives both `first_block`, an EFS block number, and
`partition_start_lba`. EFS block numbers are **partition-relative**; the mount
path says so in a comment two lines from the call. The home-block write inside
replay converted correctly:

```rust
let lba = partition_start_lba + fs_block * SECTORS_PER_BLOCK as u64;
```

The ring read, twenty lines earlier in the same function, did not:

```rust
let lba = (first_block + wrapped) * SECTORS_PER_BLOCK as u64;
```

So the ring was read `partition_start_lba` sectors too low, which on a GPT disk
is 2048 sectors before the partition even begins. Every block read was whatever
sits in front of the filesystem, no header parsed, and pass 1 broke out of its
loop on the first iteration and reported an empty ring.

The lesson is the one the v0.2.0 notes already recorded once, under "the EFS
journal wrote its ring at partition-relative LBAs": **this codebase has two
addressing domains and no type separating them.** A `u64` holding an EFS block
number and a `u64` holding an LBA are the same type, so the compiler cannot tell
a converted value from an unconverted one. That fix corrected the write side and
left the read side, in a function that had both conversions side by side.

## The second bug, found while testing the first

Pass 1 terminated only when a block failed to parse as a descriptor. It never
consulted `head_block`. On a ring that has wrapped, the blocks at and past the
head are *older* transactions that parse perfectly: intact descriptor, data and
commit blocks, with a lower sequence number. Replay would apply them, rolling
metadata backwards.

It showed up as soon as replay started working. Rewinding the tail to the very
first transaction made replay report `91 transactions (671 ring blocks)` against
a `head_block` of 644, and the resulting filesystem could not load
`bin/edos-init`. The scan is now bounded by the tail-to-head distance, wrapping,
with equal cursors meaning a full ring rather than an empty one, since
`head_seq == tail_seq` has already returned by that point.

## Reasoning rules going forward

- **A block number is not an LBA.** Anything crossing into `block_io` takes an
  absolute LBA; anything from a superblock, an extent map or a journal
  superblock is partition-relative. Convert at exactly one place per function
  and put the two conversions next to each other so a missing one is visible.
- **Replay is bounded by the head, not by parse failure.** Ring space past the
  head holds data that parses. Termination has to come from the cursors.
- **A recovery path that never runs is not tested by the suite passing.**
  `fs-regression` passes across a reboot on both filesystems and never exercised
  this, because a clean unmount has nothing to replay. To test recovery, rewind
  the journal superblock on the host: find `EJS!` at a 4 KiB boundary, set
  `tail_seq`/`tail_block` back to a real descriptor's sequence and ring index,
  recompute the CRC32 over the 64-byte struct with the field zeroed, and boot.
  Rewind to a transaction **inside** `head_block`; going further tests the
  over-scan case instead of the recovery case.
- **Ring indices exclude the superblock.** `tail_block` and `head_block` are
  indices where 0 maps to the journal region's *second* block, since block 0 is
  the superblock. Off-by-one here reads the middle of a transaction and looks
  exactly like a corrupt ring.
