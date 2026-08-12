#!/usr/bin/env python3
"""Find every copy of an fsbench pattern file inside a raw disk image.

fsbench fills its files with a position-dependent pattern (`byte_at` in
programs/fsbench/src/workloads.rs), so any 4096-byte block of one of those
files can be recognised from its first bytes alone and checked against what it
should hold. That makes it possible to answer "is the damage on the disk?"
from the host, without trusting the guest's read path — which is the half of
a data-loss bug that is hardest to rule out from inside the guest.

The image must be raw; sata-disk.img is qcow2, so convert it first (fast, and
the raw file is sparse):

    qemu-img convert -O raw sata-disk.img ~/.cache/tmp/sata.raw
    scripts/fsbench-pattern-scan.py ~/.cache/tmp/sata.raw --tag 7 --size 16M

Each mode's file has its own tag: the readahead file is 7, and `fragprep`'s
decoy file is 5, so a block found in the image belongs to exactly one of them.
Blocks a previous run left behind are still recognised, so on a disk that is not
freshly formatted the count can exceed the file's block count. What matters is the two lists it prints: a partially written
block appears in the mismatch list with the exact range of bytes that differ,
and a logical block with no copy anywhere in the image appears in the missing
list. The second one is what separates a lost write from a misdirected read:
a block the guest reads as zeros but whose pattern is on the disk was written
and read back from the wrong place, and one that is nowhere in the image was
never written at all.
"""

import argparse
import mmap

BLOCK = 4096


M64 = 0xFFFFFFFFFFFFFFFF


def byte_at(tag: int, pos: int) -> int:
    """splitmix64's finalizer, byte for byte the same as `byte_at` in
    programs/fsbench/src/workloads.rs. Change one and the other stops
    recognising the image."""
    x = (pos + (tag + 1) * 0x9E3779B97F4A7C15) & M64
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & M64
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & M64
    return (x ^ (x >> 31)) & 0xFF


def block_bytes(tag: int, offset: int, length: int = BLOCK) -> bytes:
    return bytes(byte_at(tag, offset + i) for i in range(length))


def parse_size(text: str) -> int:
    units = {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30}
    if text and text[-1].upper() in units:
        return int(text[:-1]) * units[text[-1].upper()]
    return int(text)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("image", help="raw disk image")
    ap.add_argument("--tag", type=int, default=7, help="fsbench pattern tag (the ra file is 7, fragprep's decoy is 5)")
    ap.add_argument("--size", default="16M", help="size of the pattern file")
    ap.add_argument("--limit", type=int, default=20, help="mismatching blocks to print")
    args = ap.parse_args()

    blocks = parse_size(args.size) // BLOCK
    # 16 bytes is enough to name a logical block: the pattern is a hash of the
    # byte's file offset, so two offsets sharing 16 bytes is not something this
    # generator produces over a 16 MiB file.
    index = {block_bytes(args.tag, lb * BLOCK, 16): lb for lb in range(blocks)}
    # A generator whose period divides the file size silently collapses logical
    # blocks onto one signature, and every block it swallows is then reported as
    # missing from the image. That is a bug in the pattern, not in the disk.
    if len(index) != blocks:
        raise SystemExit(
            f"pattern is not unique per block: {len(index)} signatures for {blocks} blocks. "
            "byte_at repeats within the file; fix it here and in workloads.rs together."
        )

    with open(args.image, "rb") as f:
        m = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        found = perfect = 0
        shown = 0
        copies = [0] * blocks
        for pos in range(0, len(m) - BLOCK + 1, BLOCK):
            lb = index.get(m[pos : pos + 16])
            if lb is None:
                continue
            found += 1
            copies[lb] += 1
            want = block_bytes(args.tag, lb * BLOCK)
            got = m[pos : pos + BLOCK]
            if got == want:
                perfect += 1
                continue
            bad = [i for i in range(BLOCK) if got[i] != want[i]]
            if shown < args.limit:
                shown += 1
                zeros = sum(1 for i in bad if got[i] == 0)
                print(
                    f"logical block {lb} at image offset {pos:#x} (lba {pos // 512}): "
                    f"{len(bad)} bytes differ, {zeros} of them zero, "
                    f"first {bad[0]}, last {bad[-1]}"
                )
        print(f"{found} pattern blocks in the image, {perfect} byte-perfect, {found - perfect} damaged")
        missing = [lb for lb in range(blocks) if copies[lb] == 0]
        print(f"{len(missing)} of {blocks} logical blocks have no copy anywhere in the image")
        for lb in missing[: args.limit]:
            print(f"  logical block {lb} (file offset {lb * BLOCK}) is nowhere in the image")
        # On a freshly formatted disk one logical block should appear exactly
        # once. A second copy means its bytes were also written somewhere they
        # do not belong, which is what separates a write that was dropped from
        # one that landed at the wrong address.
        dup = [lb for lb in range(blocks) if copies[lb] > 1]
        print(f"{len(dup)} of {blocks} logical blocks have more than one copy in the image")
        for lb in dup[: args.limit]:
            print(f"  logical block {lb} (file offset {lb * BLOCK}) has {copies[lb]} copies")


if __name__ == "__main__":
    main()
