# A zeroed copy of a new block overwrote the data written to it

## Status

Fixed in `041b98fc`. EFS block allocation takes a `NewBlock`
(`kernel/src/fs/efs/mod.rs`). `flush_page`, `flush_pages_bulk`,
`write_via_extents` for a full-block write, and both directory writers pass
`NewBlock::Overwritten`, which stages no zeros. `NewBlock::Zeroed` remains for
sub-block writes, which read the block back before merging into it.

## Symptoms

`fsbench fragprep /var`, reboot, `fsbench ra /var`:

```
VERIFY FAIL  byte 3801088 of the file is 0x00, want 0xd0; the chunk differs in
             20398 of 65536 bytes, 20398 of them zero, from byte 3801088 to
             byte 3858431 of the file
```

Solid runs of zeros, 4 KiB-aligned, with `extent_holes` 0 (nothing was
planned as a hole). The contiguous arm (`fsbench raprep`) verified clean in
the same build, so it took the interleaved append plus `fsync` pattern.
`fragprep` also printed `sys_sync: journal still pending after 8 rounds`.

A host scan of the image (per file tag) found blocks missing entirely, not
torn or misdirected:

```
tag 7 (fsbench.ra)    3807 pattern blocks, 3807 byte-perfect, 0 damaged
                      289 of 4096 logical blocks have no copy anywhere
tag 5 (fsbench.frag)  3918 pattern blocks, 3918 byte-perfect, 0 damaged
                      178 of 4096 logical blocks have no copy anywhere
```

The missing blocks came in stride-4 runs of seven or eight, one 4 KiB block in
every 16 KiB. `efs_stats.blocks_allocated` covered every block: allocation
happened, the data did not survive.

## Root cause

A newly allocated block was zeroed through the journal before the data was
written to it, so the journal held a copy of that home block full of zeros.
File data bypasses the block page cache and goes straight to the device, so
the two copies raced, and the zeros could land last through two paths:

- a concurrent `BlockPageCache::flush_dirty_once`, the checkpoint the other
  file's `fsync` drives, which is why only the interleaved arm lost blocks;
- ring replay on the next mount, for a transaction committed but not
  checkpointed, which is the state the `journal still pending` warning reports.

`reap_write` invalidating the block page cache after the data write closes
part of the first window and none of the second: it cannot reach the ring copy.

After the fix, both files scan whole, the file is exactly as fragmented as
before, and `fragprep`'s journal traffic fell to `ring_blocks +879 /
data_blocks +621` for 8194 allocations, since the zeroing writes were most of
the ring traffic.

## Reasoning rules going forward

- A block written on a path that bypasses a cache must never have a copy of
  itself staged in that cache or in the journal. Zero-fill on allocation is
  the natural way to write this bug, because the zeros look harmless.
- An instrument has to be proven able to see the failure before its "clean"
  means anything. Two did not, below.

## Instruments that said otherwise first

- `ra_check_edges` compared only the first and last 512 bytes of each 64 KiB
  call, so a failing offset said where the check looked, not where the damage
  was. It now walks the whole chunk once an edge fails and reports the
  differing extent.
- The fsbench pattern `byte_at` was a multiply followed by a bit slice, which
  has a 2 MiB period. A read misdirected by a multiple of 2 MiB verified as
  correct, and the scanner's signature index collapsed to 512 entries, so it
  reported 3584 blocks missing that were present. `byte_at` is now splitmix64's
  finalizer in both `programs/fsbench/src/workloads.rs` and
  `scripts/fsbench-pattern-scan.py`; the two must change together. The scanner
  refuses to run when its index holds fewer signatures than the file has
  blocks.
- Both files once shared one pattern tag, so one file's copy of an offset stood
  in for the other's missing block and the loss read as "not on the disk".
  `frag_prepare` writes the decoy with `FRAG_TAG` (5); the readahead file uses
  `RA_TAG` (7).

## If this reappears

1. Aligned solid zeros on a verified read, `extent_holes` 0.
2. Scan the image from the host. For the SATA qcow2:
   ```
   qemu-img convert -O raw sata-disk.img ~/.cache/tmp/sata.raw
   scripts/fsbench-pattern-scan.py ~/.cache/tmp/sata.raw --tag 7 --size 16M
   ```
   `nvme-disk.img`, the default root, is already raw. Scan whichever image
   held `/var`.
3. Blocks with no copy anywhere, and none partial or duplicated, mean a write
   was undone whole: look for a staged copy of a home block (block page cache
   or journal) reaching the disk after a direct write.
