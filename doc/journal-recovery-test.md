# Testing journal recovery

Recovery is the one filesystem path no ordinary test reaches. A clean unmount
leaves nothing to replay, so `fs-regression` passing across a reboot says
nothing about it; and writeback checkpoints so promptly that a power cut taken
at an arbitrary moment also finds the journal empty. Replay was broken for the
project's whole life and every test still passed
(`doc/bugs/2026-08-12-journal-replay-read-the-wrong-lba.md`).

Two things have to be arranged deliberately, and the instruments exist for
both:

- **Committed transactions still in the ring at the moment of the cut.**
  `/dev/journal-ctl` stops writeback from checkpointing journalled blocks, so
  they accumulate. Built only under the `fault-inject` kernel feature.
- **A ring that has actually wrapped**, if the wrapped-region path is what is
  under test. `journal-test.img` has a 256-block ring instead of the default
  4095, so a metadata workload wraps it in seconds. A normal boot uses about 50
  ring blocks, which is why this never happens by accident.
- **A scratch image the run has not already been through.** `make
  recovery-check` and `make orphan-check` reformat it before every run, and
  that is load-bearing rather than tidiness: both cut power mid-write and leave
  their files behind, and `recovery-check` creates `/mnt/rec_a`, `rec_b` and
  `rec_c`. On a second run those already exist, so `touch` only restamps them,
  no metadata transaction is left uncheckpointed, and the check correctly
  reports that its own setup failed rather than passing on nothing. It behaved
  exactly that way for a while: green the first time after `journal-test.img`
  was built, and red on every run after, on unchanged code. Driving the VM by
  hand per the procedure below has the same trap — reformat between attempts,
  or use file names the last attempt did not.

## Procedure

```bash
make run-recovery                       # ISO with fault-inject + the small-ring disk
scripts/edos-vm log -n 20               # wait for the desktop
```

In the guest (`scripts/edos-vm type '...' --enter`, after a `click` to focus a
terminal):

```sh
mount                                   # find the JTEST partition's device/index
mount 1 0 /mnt efs                      # device_id and partition_idx from above
cat /proc/journal_stats                 # note the `journal dev N:` row
```

`wrapped` is a property of the **live** region, not of the ring's history.
`head_block` and `tail_block` are monotonic counters, so a ring that has
physically cycled many times still reports `wrapped false` whenever `used` is
0, which is the normal state: writeback retires transactions as fast as they
commit. `fsbench write /mnt -m 8` takes a 255-block ring past `head_block 1300`
and still leaves `used 0`.

What the wrapped case needs is a live region that straddles the ring end, so
read the tail's position within the ring, `tail_block % ring_size`, and note
that the region wraps only once

    used > ring_size - (tail_block % ring_size)

Ring cost is driven by the number of **distinct metadata blocks** a transaction
touches, not by the number of operations: `touch` on ten files in one directory
hits the same inode-table, bitmap and directory blocks and merges into a single
8-block transaction. Spreading the work across many files is what consumes
ring; `fsbench write` averages about 14 ring blocks per commit.

```sh
echo pause > /dev/journal-ctl           # checkpointing stops here
touch /mnt/a /mnt/b /mnt/c              # these commit but never reach home blocks
cat /proc/journal_stats                 # `pending` and `tracked` should climb
```

Then cut power without a shutdown, which is what makes the mount unclean:

```bash
scripts/edos-vm qmp quit
```

Reboot and mount the same disk again. Replay runs on mount; the serial log
names what it applied. The result to check is that the files created after the
pause are present and the tree is intact, not merely that mount succeeded.

## Traps

- **Pause late, not for the whole workload.** Checkpointing is the only thing
  that reclaims ring space, so a journal paused while the ring fills wedges
  writes: a commit that cannot find room checkpoints and re-checks
  `CHECKPOINT_ATTEMPTS` times and then fails. That is by design, but it looks
  like a hang if you were not expecting it.
- **`wrapped` is a real precondition, not a formality.** Without it the test
  exercises the ordinary linear-region path, which was never the broken one.
- **The test disk must not carry the root's partition GUID.** Root selection
  matches the cmdline GUID across every enumerated partition, so a duplicate
  makes which disk boots a race. `journal-test.img` uses its own.

## The host-side alternative, and its limits

The journal superblock can also be rewound on the host, which forces a replay
without needing the guest to cooperate:

```bash
qemu-img convert -f qcow2 -O raw sata-disk.img /tmp/sata.raw
# find b'EJS!' at a 4096-aligned offset; u32 magic/version/block_count/block_size,
# then u64 tail_seq@16 head_seq@24 tail_block@32 head_block@40, u32 crc32@48.
# CRC is zlib.crc32 over the 64-byte struct with crc32 zeroed.
# Set tail_seq/tail_block to a real descriptor's seq and ring index, then:
qemu-img convert -f raw -O qcow2 /tmp/sata.raw sata-disk.img
```

Two traps in that, both of which cost a boot:

- **Ring indices exclude the superblock.** Index 0 maps to the journal region's
  *second* block. Setting `tail_block` to the descriptor's physical block
  number lands one block late, mid-transaction, and reads exactly like a
  corrupt ring.
- **Rewind to a transaction inside `head_block`.** Going further tests the
  over-scan bug rather than recovery: rewinding to seq 1 replayed 91
  transactions over a 644-block head and left a tree that could not load
  `bin/edos-init`.

It cannot replace the in-guest procedure for the wrapped case. A rewind can
only point at transactions the ring already holds, so on a ring that never
wrapped there is no wrapped region to construct; and pointing the tail at
transactions that were already checkpointed replays stale metadata over newer,
which is corruption rather than recovery.
