# EFS: EDOS Filesystem Specification

Version 1.0 | Status: Draft

---

## Table of Contents

1. [Overview](#1-overview)
2. [Disk Layout](#2-disk-layout)
3. [Superblock](#3-superblock)
4. [Block Group Descriptor](#4-block-group-descriptor)
5. [Inode](#5-inode)
6. [Extents](#6-extents)
7. [Directory Entries](#7-directory-entries)
8. [Symlinks](#8-symlinks)
9. [Checksums](#9-checksums)
10. [Feature Flags](#10-feature-flags)
11. [Block Size and Addressing](#11-block-size-and-addressing)
12. [Block Groups](#12-block-groups)
13. [Allocation Strategy](#13-allocation-strategy)
14. [Journal](#14-journal)
15. [Design Decisions and Rationale](#15-design-decisions-and-rationale)

---

## 1. Overview

EFS (EDOS Filesystem) is the native filesystem for EDOS. It is conceptually "ext2 with extents": a block-group-based filesystem with a flat inode table, bitmaps for allocation tracking, and extents (contiguous block runs) in place of indirect block pointer trees.

**Key properties:**

- Block groups with per-group bitmaps and inode tables
- Extent-based block mapping (no indirect blocks)
- Inline data: files up to 176 bytes are stored entirely within the inode
- 256-byte inodes with 64-bit timestamps (nanosecond precision)
- CRC32 checksums on all major on-disk structures
- Feature flag system for forward and backward compatibility
- Designed for SSD/NVMe storage, with no rotational-media optimizations
- Metadata-only write-ahead journal, mandatory (`INCOMPAT_JOURNAL`)

**What EFS is not:**

- Not a copy-on-write filesystem (no snapshots, no CoW semantics)
- Not a data journal; file data is not crash-safe (`data=writeback` semantics)
- Not optimized for spinning disk (no cylinder groups, no seek-time heuristics)

**Version:** This document describes on-disk format version 1 (`superblock.version == 1`).

---

## 2. Disk Layout

EFS divides the disk into fixed-size blocks. Block 0 is always reserved. The superblock lives at block 1. The Block Group Descriptor (BGD) table follows at block 2 and spans as many consecutive blocks as needed to hold all group descriptors.

The remainder of the disk is divided into block groups, each containing a block bitmap, an inode bitmap, an inode table, and data blocks.

```
Block 0:        Reserved (boot sector / partition header area, never read by EFS)
Block 1:        Primary Superblock (4096 bytes on disk; 256 bytes used, rest zero-padded)
Block 2+:       Block Group Descriptor Table (one 64-byte entry per group, padded to block boundary)

+-- Block Group 0 ---------------------------------------------------------+
|  [Superblock backup]          (block group 0 only, at block after BGD)   |
|  [BGD table backup]           (block group 0 only, immediately after SB) |
|  Block bitmap                 (1 block)                                   |
|  Inode bitmap                 (1 block)                                   |
|  Inode table                  (inodes_per_group * inode_size / block_size blocks)
|  Data blocks                  (remainder of group)                        |
+--------------------------------------------------------------------------+

+-- Block Group 1 ---------------------------------------------------------+
|  [Superblock backup]          (block group 1 qualifies)                   |
|  [BGD table backup]           (block group 1 qualifies)                   |
|  Block bitmap                                                             |
|  Inode bitmap                                                             |
|  Inode table                                                              |
|  Data blocks                                                              |
+--------------------------------------------------------------------------+

+-- Block Group 2 ---------------------------------------------------------+
|  Block bitmap                 (no backup; group 2 does not qualify)       |
|  Inode bitmap                                                             |
|  Inode table                                                              |
|  Data blocks                                                              |
+--------------------------------------------------------------------------+

... (repeat for all groups)
```

### Superblock Backup Locations

Superblock and BGD table backups are stored at the start of block groups whose group number is 0, 1, or a power of 3, 5, or 7:

```
Groups with backups: 0, 1, 3, 5, 7, 9, 25, 27, 49, 125, 243, 343, ...
```

A group number `n > 1` qualifies if and only if `n` is a positive power of 3, 5, or 7 (i.e., `n == 3^k`, `n == 5^k`, or `n == 7^k` for some integer `k >= 1`).

In qualifying groups other than group 0, the backup occupies the first blocks of the group in the order: superblock (1 block), BGD table (same number of blocks as the primary). These blocks are accounted for in the group's block bitmap as allocated.

---

## 3. Superblock

The primary superblock is stored at block 1. It occupies a full block on disk (4096 bytes for the default 4K block size), but only the first 256 bytes are defined. The remaining bytes must be zero.

All multi-byte fields are stored **little-endian**.

| Offset | Size | Field | Type | Description |
|--------|------|-------|------|-------------|
| 0 | 4 | `magic` | `u32` | Magic number: `0x45465321` (ASCII "EFS!") |
| 4 | 4 | `version` | `u32` | On-disk format version. Must be `1` for this spec |
| 8 | 8 | `total_blocks` | `u64` | Total number of blocks in the filesystem |
| 16 | 8 | `total_inodes` | `u64` | Total number of inodes allocated across all groups |
| 24 | 8 | `free_blocks` | `u64` | Current count of free (unallocated) blocks |
| 32 | 8 | `free_inodes` | `u64` | Current count of free (unallocated) inodes |
| 40 | 4 | `block_size_log2` | `u32` | `log2(block_size_in_bytes)`. Value 12 = 4096 bytes, 13 = 8192 bytes |
| 44 | 4 | `blocks_per_group` | `u32` | Number of blocks per block group |
| 48 | 4 | `inodes_per_group` | `u32` | Number of inodes per block group |
| 52 | 2 | `inode_size` | `u16` | Size of each inode in bytes. Must be `256` for v1 |
| 54 | 2 | `block_group_count` | `u16` | Total number of block groups |
| 56 | 8 | `compatible_features` | `u64` | Compatible feature flag bitmask (see Section 10) |
| 64 | 8 | `incompatible_features` | `u64` | Incompatible feature flag bitmask (see Section 10) |
| 72 | 8 | `read_only_features` | `u64` | Read-only compatible feature flag bitmask (see Section 10) |
| 80 | 16 | `uuid` | `[u8; 16]` | Filesystem UUID (RFC 4122 format recommended) |
| 96 | 64 | `volume_name` | `[u8; 64]` | Null-terminated UTF-8 volume label. Unused bytes must be zero |
| 160 | 8 | `mount_time` | `u64` | Unix timestamp (seconds since epoch) of the last mount |
| 168 | 8 | `write_time` | `u64` | Unix timestamp (seconds since epoch) of the last write |
| 176 | 2 | `mount_count` | `u16` | Number of times mounted since last fsck |
| 178 | 2 | `max_mount_count` | `u16` | Recommended maximum mount count before fsck. `0` = no limit |
| 180 | 8 | `first_data_block` | `u64` | Block number of the first block not used by superblock or BGD table. Typically `2 + bgd_table_block_count` |
| 188 | 4 | `checksum` | `u32` | CRC32 of the first 256 bytes of the superblock with this field zeroed during computation |
| 192 | 8 | `journal_first_block` | `u64` | First block of the journal extent, absolute within the partition. Valid only when `INCOMPAT_JOURNAL` is set (see Section 14) |
| 200 | 4 | `journal_block_count` | `u32` | Blocks reserved for the journal, including its superblock |
| 204 | 4 | `journal_sb_checksum` | `u32` | CRC32 of the journal superblock, so it can be sanity-checked without reading the journal |
| 208 | 1 | `fsck_in_progress` | `u8` | Set to 1 while `efs-fsck --repair` runs, cleared on clean exit. If set at startup, fsck refuses to run without `--force` |
| 209 | 47 | `reserved` | `[u8; 47]` | Reserved; must be zero |

**Total defined size:** 256 bytes. Bytes 256 through end-of-block must be zero.

### Superblock Validation

A driver must verify the following before mounting:

1. `magic == 0x45465321`
2. `version == 1` (or a version the driver knows how to handle)
3. `inode_size == 256`
4. `block_size_log2` is one of: 10, 11, 12, 13
5. `checksum` matches the computed CRC32 (warn on mismatch; behavior in v1 is to warn and continue, since no fsck tool exists yet)
6. Feature flag checks (see Section 10)

---

## 4. Block Group Descriptor

The Block Group Descriptor (BGD) table starts at block 2 and contains one 64-byte entry per block group, stored consecutively. The table is zero-padded to fill complete blocks.

Each entry describes the layout of one block group.

All multi-byte fields are stored **little-endian**.

| Offset | Size | Field | Type | Description |
|--------|------|-------|------|-------------|
| 0 | 8 | `block_bitmap_block` | `u64` | Absolute block number of this group's block allocation bitmap |
| 8 | 8 | `inode_bitmap_block` | `u64` | Absolute block number of this group's inode allocation bitmap |
| 16 | 8 | `inode_table_block` | `u64` | Absolute block number of the first block of this group's inode table |
| 24 | 8 | `free_blocks_count` | `u64` | Number of free (unallocated) blocks in this group |
| 32 | 8 | `free_inodes_count` | `u64` | Number of free (unallocated) inodes in this group |
| 40 | 8 | `used_dirs_count` | `u64` | Number of directory inodes allocated in this group |
| 48 | 4 | `checksum` | `u32` | CRC32 of this 64-byte descriptor with this field zeroed during computation |
| 52 | 12 | `reserved` | `[u8; 12]` | Reserved; must be zero |

**Total size:** 64 bytes per entry.

### BGD Table Size

```
bgd_table_bytes = block_group_count * 64
bgd_table_blocks = ceil(bgd_table_bytes / block_size)
```

The BGD table occupies blocks 2 through `2 + bgd_table_blocks - 1` inclusive.

### Block Bitmap Layout

The block bitmap is one block in size. Each bit corresponds to one block within the group: bit `i` covers the block at `group_start + i`. A `1` bit means the block is **allocated**; a `0` bit means it is **free**.

If `blocks_per_group` is not a multiple of 8, the trailing bits in the last byte of the bitmap must be set to `1` (treated as allocated) to prevent accidental allocation beyond the group boundary.

### Inode Bitmap Layout

The inode bitmap is one block in size. Each bit corresponds to one inode slot within the group. Bit `i` covers inode number `group * inodes_per_group + i + 1`. A `1` bit means the inode is **allocated**; `0` means **free**.

Trailing bits (if `inodes_per_group` is not a multiple of 8) must be set to `1`.

---

## 5. Inode

An inode describes one file, directory, or symlink. All inodes are 256 bytes. They are stored consecutively in the inode table of their block group.

All multi-byte fields are stored **little-endian**.

| Offset | Size | Field | Type | Description |
|--------|------|-------|------|-------------|
| 0 | 2 | `mode` | `u16` | File type (upper 4 bits) + Unix permission bits (lower 12 bits) |
| 2 | 2 | `uid` | `u16` | Owner user ID |
| 4 | 2 | `gid` | `u16` | Owner group ID |
| 6 | 2 | `link_count` | `u16` | Hard link count. Inode is freed when this reaches 0 |
| 8 | 8 | `size` | `u64` | File size in bytes |
| 16 | 8 | `blocks` | `u64` | Number of filesystem blocks (not 512-byte sectors) allocated to this inode |
| 24 | 4 | `flags` | `u32` | Inode flags (see Inode Flags table below) |
| 28 | 4 | `reserved1` | `u32` | Reserved; must be zero |
| 32 | 8 | `ctime_sec` | `u64` | Creation time: seconds since Unix epoch (1970-01-01 00:00:00 UTC) |
| 40 | 4 | `ctime_nsec` | `u32` | Creation time: nanoseconds component (0..=999999999) |
| 44 | 4 | `reserved2` | `u32` | Reserved; must be zero |
| 48 | 8 | `mtime_sec` | `u64` | Modification time: seconds since Unix epoch |
| 56 | 4 | `mtime_nsec` | `u32` | Modification time: nanoseconds component |
| 60 | 4 | `reserved3` | `u32` | Reserved; must be zero |
| 64 | 8 | `atime_sec` | `u64` | Access time: seconds since Unix epoch |
| 72 | 4 | `atime_nsec` | `u32` | Access time: nanoseconds component |
| 76 | 4 | `checksum` | `u32` | CRC32 of the full 256-byte inode with this field zeroed during computation |
| 80 | 176 | `data_area` | `[u8; 176]` | Extent tree root OR inline data (interpretation determined by `flags`) |

**Total size:** 256 bytes.

### data_area Interpretation

The `data_area` is interpreted differently depending on the `INLINE_DATA` flag:

**When `INLINE_DATA` is NOT set (extent mode, normal files and directories):**

```
data_area[0..12]   = EfsExtentHeader   (see Section 6)
data_area[12..176] = EfsExtent entries (leaf, depth=0) or EfsExtentIndex entries (depth>0)
                     Up to 13 entries of 12 bytes each
```

**When `INLINE_DATA` IS set:**

```
data_area[0..176] = raw file content
inode.size        = number of valid bytes (0..=176)
```

The extent tree is not present when `INLINE_DATA` is set. Files stored inline have no allocated blocks (`blocks == 0`).

### Inline-to-Extent Transition

When a write would cause an inline file's size to exceed 176 bytes, the driver must perform the following steps atomically within the write path:

1. Allocate one or more data blocks sufficient to hold the current inline content plus the new data
2. Copy the existing inline bytes to the start of the first allocated block
3. Write the new data at the appropriate offset
4. Build an `EfsExtentHeader` and one `EfsExtent` entry in `data_area`
5. Clear the `INLINE_DATA` flag in `inode.flags`
6. Update `inode.size` and `inode.blocks`
7. Write the updated inode

The transition must not leave the inode in a state where `INLINE_DATA` is clear but `data_area` contains neither a valid extent header nor coherent extent entries.

### File Type Encoding (mode field, upper 4 bits)

The upper 4 bits of `mode` (bits 15:12) encode the file type:

| Upper 4 bits | Full `mode` mask | Constant | Meaning |
|---|---|---|---|
| `0x8` | `0x8000` | `S_IFREG` | Regular file |
| `0x4` | `0x4000` | `S_IFDIR` | Directory |
| `0xA` | `0xA000` | `S_IFLNK` | Symbolic link |

The lower 12 bits of `mode` are standard Unix permission bits (setuid, setgid, sticky, rwxrwxrwx).

To check file type: `(inode.mode & 0xF000) == S_IFREG` (etc.)

### Inode Flags

| Bit | Name | Value | Meaning |
|-----|------|-------|---------|
| 0 | `INLINE_DATA` | `0x00000001` | File data is stored inline in `data_area`; no extent tree |
| 1 | `IMMUTABLE` | `0x00000002` | File content and metadata cannot be modified; attempts return `EPERM` |
| 2..31 | (reserved) | | Must be zero in v1; preserve on write |

### Reserved Inodes

| Inode Number | Purpose |
|---|---|
| 0 | Invalid / null. Never allocated, never stored on disk as a valid inode |
| 1 | Root directory of the filesystem |

The constant `EFS_ROOT_INO = 1` refers to the root directory inode.

Inode numbering starts at 1. The location of inode N on disk is:

```
group       = (N - 1) / inodes_per_group
index       = (N - 1) % inodes_per_group
byte_offset = bgd[group].inode_table_block * block_size + index * inode_size
```

The inode at byte_offset is the full 256-byte inode structure for inode N.

---

## 6. Extents

EFS uses a tree structure to map a file's logical block offsets to physical block addresses. The tree root is embedded in the inode's `data_area`. Internal (non-leaf) nodes point to child blocks that contain further headers and entries. Leaf nodes contain the actual physical block mappings.

All multi-byte fields are stored **little-endian**.

A logical block below the file's size need not be mapped. Such a block is a hole: it reads as zeroes and occupies no space, and it is not counted in `inode.blocks`. Growing a file with `truncate` is what creates one, since the new size is stamped without allocating the blocks it names. Writing to a hole allocates its block.

### 6.1 Extent Header (12 bytes)

The extent header appears at the beginning of `data_area` in the inode and at the beginning of every extent tree node block.

| Offset | Size | Field | Type | Description |
|--------|------|-------|------|-------------|
| 0 | 2 | `magic` | `u16` | `0xEF10`. Validates this is an extent header |
| 2 | 2 | `entries` | `u16` | Number of valid extent or index entries following this header |
| 4 | 2 | `max_entries` | `u16` | Maximum number of entries that can fit after this header |
| 6 | 2 | `depth` | `u16` | Tree depth at this node. `0` = leaf (entries are `EfsExtent`). `> 0` = internal (entries are `EfsExtentIndex`) |
| 8 | 4 | `reserved` | `u32` | Reserved; must be zero |

**Inode root node:** `max_entries` is always `13` (since `(176 - 12) / 12 = 13`).

**Block-resident nodes:** `max_entries` is `(block_size - 12) / 12`. For 4K blocks: `(4096 - 12) / 12 = 340`.

### 6.2 Extent Entry (12 bytes, leaf node, depth=0)

Each extent entry maps a contiguous range of logical blocks to a contiguous range of physical blocks.

| Offset | Size | Field | Type | Description |
|--------|------|-------|------|-------------|
| 0 | 4 | `logical_block` | `u32` | Starting logical block number within the file |
| 4 | 2 | `length` | `u16` | Number of blocks in this extent. The high bit (bit 15) is reserved and must be 0 in v1. Valid range: `1..=32767` |
| 6 | 2 | `start_hi` | `u16` | Bits 47:32 of the physical starting block number |
| 8 | 4 | `start_lo` | `u32` | Bits 31:0 of the physical starting block number |

**Physical block address reconstruction:**

```
physical_start = (start_hi as u64) << 32 | start_lo as u64
```

This gives a 48-bit physical block address, supporting up to 2^48 blocks (1 exabyte at 4K block size).

**Coverage:** This extent covers logical blocks `logical_block` through `logical_block + length - 1`, mapped to physical blocks `physical_start` through `physical_start + length - 1`.

**Reserved high bit of `length`:** Bit 15 is reserved for future "unwritten extent" semantics (a block range that has been allocated but whose content should be read back as zeroes, used by some filesystems to implement fallocate without zeroing). In v1 this bit must be 0 and drivers must treat any extent with this bit set as an error.

### 6.3 Extent Index Entry (12 bytes, internal node, depth > 0)

Each index entry points to a child node block that covers a range of logical blocks. All child entries beginning at `logical_block` or higher (and below the next index entry's `logical_block`) are found in the subtree rooted at the child block.

| Offset | Size | Field | Type | Description |
|--------|------|-------|------|-------------|
| 0 | 4 | `logical_block` | `u32` | Lowest logical block covered by the subtree rooted at `leaf` |
| 4 | 2 | `reserved` | `u16` | Reserved; must be zero |
| 6 | 2 | `leaf_hi` | `u16` | Bits 47:32 of the child block number |
| 8 | 4 | `leaf_lo` | `u32` | Bits 31:0 of the child block number |

**Child block address reconstruction:**

```
child_block = (leaf_hi as u64) << 32 | leaf_lo as u64
```

The child block starts with an `EfsExtentHeader` followed by either more index entries (if `child.depth > 0`) or leaf extent entries (if `child.depth == 0`). The `depth` field decreases by one at each level of the tree.

### 6.4 Extent Tree Lookup Algorithm

To find the physical block for logical offset `L`:

```
node = inode.data_area
loop:
    header = parse EfsExtentHeader at node[0..12]
    assert header.magic == 0xEF10
    if header.depth == 0:
        // Leaf: binary search entries for the extent covering L
        for each EfsExtent in node[12 .. 12 + header.entries * 12]:
            if extent.logical_block <= L < extent.logical_block + extent.length:
                physical = extent.physical_start + (L - extent.logical_block)
                return physical
        return HOLE (block not mapped, read as zeroes)
    else:
        // Internal: find the index entry whose subtree contains L
        // The correct entry is the last one with logical_block <= L
        for i in (header.entries - 1) downto 0:
            if index[i].logical_block <= L:
                node = read_block(index[i].child_block)
                break
        // (if no entry found, the block is not mapped)
```

### 6.5 Supported Tree Depths

The v1 driver reads and writes `depth = 0` and `depth = 1`. A `depth > 1` node is rejected with `EOPNOTSUPP` and must not be interpreted.

The shape is chosen by how many extents the file needs, and a file moves between the two in either direction as it grows and is truncated:

- **`depth = 0`** — up to 13 extents live in `data_area` directly. No tree blocks are allocated.
- **`depth = 1`** — `data_area` holds up to 13 `EfsExtentIndex` entries. Each names a leaf block holding an `EfsExtentHeader` plus up to `(block_size - 12) / 12` extents (340 at a 4K block size).

What each depth can address:

```
max_blocks_per_extent = 32767   (15-bit length field, high bit reserved)

depth 0: 13 extents          -> 13 * 32767 * 4096         = ~1.66 GB
depth 1: 13 * 340 = 4420     -> 4420 * 32767 * 4096       = ~565 GB
```

The depth-0 ceiling is not about file *size*: it is 13 discontiguous runs. A 1 MB file written onto fragmented free space needs more than 13 and used to fail with `EOPNOTSUPP` from `fsync` while writeback silently dropped the data. Depth 1 is what removes that.

**Leaf blocks are allocated storage.** They are marked in the block bitmap like any data block, they are freed when the file is truncated back inside the inline limit or unlinked, and `efs-fsck` counts them when rebuilding the bitmap. They are deliberately *not* counted in `EfsInode::blocks`, which records data blocks only.

The on-disk format supports trees of arbitrary depth, so raising the ceiling again needs no layout change.

---

## 7. Directory Entries

Directories are regular inodes (`S_IFDIR`) whose data blocks contain a sequence of variable-length directory entry records. The data blocks are allocated and addressed via extents, exactly as for regular files.

All multi-byte fields are stored **little-endian**.

### 7.1 Directory Entry Structure

| Offset | Size | Field | Type | Description |
|--------|------|-------|------|-------------|
| 0 | 8 | `inode` | `u64` | Inode number of the referenced file. `0` means this slot is unused/deleted |
| 8 | 2 | `rec_len` | `u16` | Total length of this record in bytes, including the header and name, rounded up to 4-byte alignment. Used to skip to the next entry |
| 10 | 1 | `name_len` | `u8` | Length of the filename in bytes. Maximum 255 |
| 11 | 1 | `file_type` | `u8` | File type hint; see table below. Redundant with the inode mode but avoids reading the inode during directory listing |
| 12 | N | `name` | `[u8]` | Filename bytes. NOT null-terminated. Valid range: `name[0..name_len]` |

**Minimum record size:** `12 + name_len` bytes, rounded up to the next 4-byte boundary.

```
min_rec_len = (12 + name_len + 3) & !3
```

Records are always 4-byte aligned. The `name` field may be followed by 0 to 3 bytes of padding to reach alignment; padding bytes are undefined and must not be read.

### 7.2 Directory Entry Rules

- **No cross-block entries:** A directory entry must fit entirely within one block. No entry spans a block boundary. If the remaining space in a block is smaller than the minimum size for a new entry (12 bytes minimum, 16 bytes to hold a 1-byte name), it must be absorbed into the last entry's `rec_len`.
- **Last entry in block:** The last valid entry in each data block has its `rec_len` extended to reach the end of the block. If the remaining space after the last entry is not enough for any new entry, it is included in the last entry's `rec_len` as padding.
- **Deleted entries:** When an entry is deleted, its `inode` field is set to `0`. The space is reclaimed by extending the `rec_len` of the preceding entry to include the deleted entry's space. If the deleted entry is the first entry in a block, it remains in place with `inode = 0` as a gap until the driver coalesces it.
- **Minimum filename:** Filenames must be at least 1 byte. The filename `.` must not exceed 1 byte; `..` must not exceed 2 bytes.
- **No null bytes in names:** Filename bytes must not be `0x00`. Names are compared byte-for-byte (case-sensitive).
- **Mandatory entries:** Every directory must contain exactly one `.` entry (inode = self) and exactly one `..` entry (inode = parent). For the root directory, `..` points to inode 1 (itself).
- **Directory size:** `inode.size` reflects the total byte length of the directory's data (sum of all allocated block bytes actually used for entries). Drivers may round this up to a full block.

### 7.3 Directory Entry File Type Values

| Value | Constant | Meaning |
|-------|----------|---------|
| 0 | `FT_UNKNOWN` | Unknown file type |
| 1 | `FT_REG_FILE` | Regular file |
| 2 | `FT_DIR` | Directory |
| 7 | `FT_SYMLINK` | Symbolic link |

Values 3-6 and 8-255 are reserved. Drivers must write the correct value when creating entries and must not reject entries with unknown `file_type` values during reads (treat as `FT_UNKNOWN`).

### 7.4 Directory Traversal

To list a directory:

```
offset = 0
while offset < inode.size:
    entry = read_bytes(offset, sizeof(DirEntryHeader))  // 12 bytes
    if entry.inode != 0:
        name = read_bytes(offset + 12, entry.name_len)
        yield (entry.inode, name, entry.file_type)
    offset += entry.rec_len
    assert entry.rec_len >= 12          // prevent infinite loop
    assert entry.rec_len % 4 == 0       // alignment invariant
```

### 7.5 New Directory Initialization

When creating a new directory, the driver must write two initial entries into the first data block:

```
Entry 1: inode=self_ino, name_len=1, file_type=FT_DIR, name="."
         rec_len = 16   (12 + 1, rounded up to 4-byte alignment)

Entry 2: inode=parent_ino, name_len=2, file_type=FT_DIR, name=".."
         rec_len = block_size - 16   (extends to end of block)
```

The parent directory's `used_dirs_count` in the BGD is incremented.

---

## 8. Symlinks

A symbolic link stores a target path string.

**Short symlinks (target path length <= 176 bytes):**
- Stored entirely within the inode's `data_area`
- `inode.flags` has `INLINE_DATA` set
- `inode.size` = number of bytes in the target path string (NOT null-terminated; length is in `size`)
- `data_area[0..inode.size]` contains the raw UTF-8 target path
- `inode.blocks` = 0 (no data blocks allocated)

**Long symlinks (target path length > 176 bytes):**
- Stored in data blocks allocated via extents, identical to regular file data
- `inode.flags` does NOT have `INLINE_DATA` set
- `inode.size` = number of bytes in the target path string
- `inode.blocks` = number of allocated data blocks
- The target path occupies the first `inode.size` bytes of the file data

Target paths are not null-terminated on disk in either case; length is always determined by `inode.size`.

**Maximum symlink length:** Limited by the filesystem's maximum file size. There is no separate smaller limit imposed by EFS.

**Unlinking a symlink frees it immediately.** A regular file's inode and blocks
survive the removal of its last name until the last open reference goes away, but
nothing can hold a reference to a link: `open` follows it, so only the link's own
name reaches it. Removing the directory entry and freeing the inode therefore
happen in one transaction. An allocated `S_IFLNK` inode with no directory entry is
a leak, and `efs-fsck` reports it as an orphan.

---

## 9. Checksums

EFS uses CRC32 to detect accidental corruption in critical metadata structures.

**Algorithm:** CRC32 as specified in ISO 3309 (also used by Ethernet, zlib, and PNG). Polynomial: `0xEDB88320` (reflected form). Initial value: `0xFFFFFFFF`. Final XOR: `0xFFFFFFFF`.

This is the same CRC32 as computed by `crc32()` in zlib, or by hardware `CRC32` instructions on x86 (`crc32` + `_mm_crc32_u8`).

### Computing a Checksum

For each structure that has a `checksum` field:

1. Obtain the byte slice of the complete structure (e.g., all 256 bytes for the superblock, all 64 bytes for a BGD entry, all 256 bytes for an inode)
2. Copy the slice or save the current `checksum` value and set `checksum = 0` in place
3. Compute CRC32 over the entire byte slice with `checksum` zeroed
4. Store the result in the `checksum` field

### Verifying a Checksum

1. Save the stored `checksum` value
2. Zero the `checksum` field in the in-memory copy
3. Compute CRC32 over the structure
4. Compare the computed value against the saved stored value
5. If they differ: log a warning; in v1 (no fsck tool), continue mounting rather than refusing

**v1 behavior on mismatch:** Log the mismatch with the structure type, block number, and inode number (if applicable). Do not refuse to mount or return an error to userspace on read. A future fsck tool will handle repair.

### Which Structures Are Checksummed

| Structure | Checksum Coverage |
|---|---|
| Superblock | Bytes 0..255 of the on-disk superblock block (first 256 bytes) |
| Block Group Descriptor | All 64 bytes of the BGD entry |
| Inode | All 256 bytes of the inode |

Extent tree node blocks and directory entry blocks are NOT checksummed in v1. A future feature flag may add per-block checksums.

---

## 10. Feature Flags

The superblock contains three 64-bit feature bitmasks that control compatibility between different driver versions and filesystem instances.

### Three Categories

**`compatible_features`** (offset 56 in superblock):
The filesystem may have features that an older driver does not understand. An older driver can still mount the filesystem read/write safely; it just ignores those features. Unknown bits are preserved verbatim on write.

**`incompatible_features`** (offset 64 in superblock):
The filesystem has on-disk format changes that an older driver would silently corrupt if it attempted to write (or even read) without understanding them. A driver that encounters an unknown bit in this field MUST refuse to mount.

**`read_only_features`** (offset 72 in superblock):
The filesystem has metadata that an older driver would not update correctly if it wrote to the filesystem. An older driver may mount read-only if it encounters unknown bits here, but must not allow writes.

### Mount Policy (pseudocode)

```rust
const KNOWN_INCOMPAT: u64 = INCOMPAT_JOURNAL;  // 0x1
const KNOWN_RO_COMPAT: u64 = 0;                // none defined in v1

if (sb.incompatible_features & !KNOWN_INCOMPAT) != 0 {
    return Err(MountError::UnknownIncompatFeature);
}
let read_only = (sb.read_only_features & !KNOWN_RO_COMPAT) != 0;
// compatible_features: mount normally, preserve all bits on write
```

### Defined Feature Flags

#### Compatible Features (`compatible_features`)

| Bit | Name | Value | Meaning |
|-----|------|-------|---------|
| 0 | `COMPAT_DISCARD` | `0x0000000000000001` | The filesystem was created with TRIM/discard support. The driver should issue discard commands to the block device when freeing blocks. Drivers that do not support discard can ignore this flag and mount read/write. |

#### Incompatible Features (`incompatible_features`)

| Bit | Name | Value | Meaning |
|-----|------|-------|---------|
| 0 | `INCOMPAT_JOURNAL` | `0x0000000000000001` | The filesystem carries a metadata journal, described in Section 14. A driver that cannot replay the journal must refuse to mount, read-only included, since unreplayed metadata makes the home locations stale. Set on every image `efs-mkfs` produces. |

#### Read-Only Compatible Features (`read_only_features`)

None defined in v1.

### Preserving Unknown Flags

When writing the superblock, a driver must preserve all bits in all three feature fields that it did not set. In particular:

- Bits in `compatible_features` that the driver does not know about must be written back unchanged
- Bits in `read_only_features` that the driver does not know about must be written back unchanged (since it mounted read-only, it will not be writing the superblock)
- Bits in `incompatible_features` that the driver does not know about should never be reached (the driver refused to mount)

---

## 11. Block Size and Addressing

### Block Size

The block size is chosen at filesystem creation time (`mkfs.efs`) and is fixed for the lifetime of the filesystem. It is stored as `log2(block_size)` in `superblock.block_size_log2`.

| `block_size_log2` | Block size | Max filesystem size (64-bit block addr) |
|---|---|---|
| 10 | 1024 bytes (1K) | ~1.8 * 10^19 bytes |
| 11 | 2048 bytes (2K) | ~3.7 * 10^19 bytes |
| 12 | 4096 bytes (4K) | ~7.4 * 10^19 bytes (default) |
| 13 | 8192 bytes (8K) | ~1.5 * 10^20 bytes |

The default and recommended block size is 4096 bytes (log2 = 12), matching the x86-64 page size.

### Block Addresses

All block addresses in EFS metadata are `u64` values. The practical address space is 48 bits (as enforced by extent entries using `start_hi`/`start_lo` pairs), but the BGD table and inode fields use full 64-bit addresses.

Block 0 is always reserved and must never be allocated for metadata or data. Any free-space allocation algorithm must skip block 0.

### Byte Address to Block Number

```
block_number = byte_address >> block_size_log2
block_offset = byte_address & (block_size - 1)
```

### Block Number to Byte Address for I/O

```
byte_address = block_number << block_size_log2
```

---

## 12. Block Groups

### Purpose

Block groups divide the filesystem into fixed-size regions, each self-contained with its own bitmaps and inode table. This provides:

- Bounded bitmap size (one block per bitmap, known at mkfs time)
- Locality of metadata and data within a group (metadata like inodes lives near the data it describes)
- Bounded free-space scan (search only within a group before moving to the next)
- Parallel allocation potential in future multi-threaded drivers

On SSDs, the rotational-media motivation for groups (reducing seek distance) does not apply, but the organizational and bounded-scan benefits remain.

### Default Sizing

```
blocks_per_group  = block_size * 8
                  = 4096 * 8 = 32768 blocks  (for 4K blocks)
                  = 32768 * 4096 = 128 MB per group
```

This sizing ensures the block bitmap for one group fits exactly in one block (one bit per block, 8 bits per byte, so `blocks_per_group / 8 = block_size` bytes).

```
inodes_per_group  = blocks_per_group / 8
                  = 32768 / 8 = 4096 inodes  (for 4K blocks)

inode_table_blocks_per_group = inodes_per_group * inode_size / block_size
                             = 4096 * 256 / 4096 = 256 blocks
```

### Group 0 Layout Detail

Group 0 is special because it hosts the primary superblock and BGD table.

```
Block 0:                       Reserved
Block 1:                       Superblock
Blocks 2 .. 2+bgd_blocks-1:   BGD table
Block 2+bgd_blocks:            Group 0 block bitmap
Block 2+bgd_blocks+1:          Group 0 inode bitmap
Blocks 2+bgd_blocks+2 ..
       2+bgd_blocks+1+inode_table_blocks: Group 0 inode table
Remaining blocks in group 0:   Group 0 data blocks
```

The block bitmap for group 0 must mark as allocated all blocks up to and including the inode table, since they are occupied by metadata.

### Backup Groups Layout

In qualifying groups (see Section 2), the first blocks of the group are occupied by the superblock backup (1 block) and BGD table backup (`bgd_table_blocks` blocks) before the block bitmap, inode bitmap, and inode table. These blocks are marked as allocated in the group's block bitmap.

### Number of Block Groups

```
block_group_count = ceil(total_blocks / blocks_per_group)
```

The last group may have fewer than `blocks_per_group` blocks. `total_inodes` and `free_inodes` in the superblock are the sum of `inodes_per_group` across all groups (even the last, potentially partial group).

---

## 13. Allocation Strategy

### Block Allocation

**Goal:** maximize contiguous allocation to minimize extent count, which reduces metadata writes and improves sequential I/O performance.

**Algorithm for allocating `n` blocks for a file:**

1. Determine the preferred block group: use the group containing the file's existing last extent, or the group containing the parent directory inode if the file has no blocks yet.
2. In the preferred group, scan the block bitmap for a contiguous run of `n` free blocks starting after the last allocated block of the file (if any). If found, allocate that run.
3. If no contiguous run of length `n` exists, take the largest available run, then look for the remainder elsewhere.
4. If the preferred group has no free blocks, try adjacent groups, then any group with free blocks.
5. Always try to extend the file's last extent rather than starting a new one. Only create a new extent entry if the new block is not physically adjacent to the last extent's end block.

**No rotational-media heuristics:** Do not use cylinder-group locality, fragmentation defrag hints, or preallocation beyond what is needed for the current write.

### Inode Allocation

1. Prefer the block group of the parent directory when allocating a new inode.
2. Within the group, scan the inode bitmap for the first free bit.
3. If the preferred group is full, scan other groups round-robin.

### TRIM/Discard

If `COMPAT_DISCARD` is set in `compatible_features` and the block device supports discard operations:

- When freeing blocks (during file truncation, deletion, or extent removal), issue a discard command for the freed block range to the block device after updating the bitmap.
- Discard is a hint to the SSD and must not be relied upon for correctness. The block bitmap is always the authoritative source of free/allocated status.

### Free Space Thresholds

The superblock's `free_blocks` and `free_inodes` are updated atomically with every allocation and deallocation. Drivers may cache these counts in memory and write them to disk on unmount or at periodic sync points.

---

## 14. Journal

EFS uses a metadata-only write-ahead journal (WAL) for crash safety,
inspired by ext3/jbd2 in `data=writeback` mode. File data is NOT
journaled; only metadata mutations (inodes, bitmaps, BGDs, directory
data blocks) are recorded in the journal before being applied to their
home locations.

### On-disk layout

The journal occupies a contiguous extent reserved at `efs-mkfs` time
(default 16 MiB). Its location is recorded in the EFS superblock:

- `journal_first_block: u64`, first block of the journal extent
- `journal_block_count: u32`, total blocks including the journal superblock
- `journal_sb_checksum: u32`, CRC32 of the journal superblock

The `INCOMPAT_JOURNAL` (0x1) feature bit must be set. The kernel
refuses to mount images without it.

### Journal superblock (block 0 of the extent)

64 bytes, `repr(C, packed)`:

| Offset | Size | Field         | Description                              |
|--------|------|---------------|------------------------------------------|
| 0      | 4    | magic         | `JOURNAL_MAGIC`, disk bytes `45 4A 53 21` (`"EJS!"`) |
| 4      | 4    | version       | Must be 1                                |
| 8      | 4    | block_count   | Total journal blocks                     |
| 12     | 4    | block_size    | Must match FS block size (4096)          |
| 16     | 8    | tail_seq      | Oldest live transaction seq              |
| 24     | 8    | head_seq      | Next transaction seq                     |
| 32     | 8    | tail_block    | Ring offset of oldest live data          |
| 40     | 8    | head_block    | Ring offset of next write position       |
| 48     | 4    | crc32         | CRC32 of struct with this field zeroed   |
| 52     | 12   | reserved      | Must be zero                             |

### Ring structure

Block 0 = journal superblock. Blocks 1..block_count-1 form the ring
(ring_size = block_count - 1). Ring positions wrap modulo ring_size.
`journal_block_lba(idx) = (first_block + (idx % ring_size) + 1) * 8`.

### Block types

Every journal metadata block starts with a 24-byte `JournalBlockHeader`:

| Offset | Size | Field | Description                                    |
|--------|------|-------|------------------------------------------------|
| 0      | 4    | magic | `JOURNAL_BLOCK_MAGIC`, disk bytes `45 4A 42 21` (`"EJB!"`) |
| 4      | 1    | kind  | 1=Descriptor, 2=Commit, 3=Revoke              |
| 5      | 3    | _pad  | Zero                                           |
| 8      | 8    | seq   | Transaction sequence number                    |
| 16     | 8    | tx_id | Transaction identifier                         |

**Descriptor block**: lists fs_blocks whose data follows.
Layout: header (24B) + entry_count (u32, 4B) + DescriptorEntry[N].
Each DescriptorEntry is 16 bytes: `fs_block: u64`, `flags: u32`,
`_reserved: u32`. Flag bit 0 = ESCAPED (first 4 bytes replaced with
zeros because they matched JOURNAL_BLOCK_MAGIC).

**Data blocks**: one 4096-byte block per descriptor entry, in order.
Contains the metadata block content as-is (or escaped).

**Revoke block**: lists blocks that must NOT be replayed from older txs.
Layout: header (24B) + RevokeEntry[N] (16B each: `fs_block: u64`,
`seq: u64`). Terminated by zero entry.

**Commit block**: seals a transaction. Layout: header (24B) +
`payload_crc: u32` (CRC32 of all data block bytes, in escaped form).
Written with FUA for durability.

### Transaction flow

1. `TxHandle` (RAII) opened at each top-level metadata op.
2. Helpers enroll dirty pages via `tx.enroll_block(dev, block, page)`.
3. `free_block` also enrolls revoke records via `tx.enroll_revoke`.
4. TxHandle::Drop merges staged blocks into the active transaction.
5. Committer kthread (1s tick or kick) seals active -> writes
   descriptor + data + optional revoke + flush_cache + FUA commit.
6. Writeback gate: dirty pages skip flush until committed_seq >= their
   enrolled seq.
7. After checkpoint (home-location flush), advance_tail reclaims ring.

### Mount-time replay

Two-pass scan from tail_block:
- Pass 1: collect committed txs and revoke set (fs_block -> max_revoke_seq)
- Pass 2: apply data blocks to home locations, skipping revoked blocks
  (revoke_seq >= tx_seq). Un-escape DESC_FLAG_ESCAPED blocks.

After replay: flush_cache, write updated JSB (tail=head) with FUA.
Idempotent: crash during replay re-replays on next boot.

### Crash safety guarantees

- Metadata committed to the journal survives power loss.
- Partial transactions (no commit block) are discarded on replay.
- Freed blocks are revoke-protected against stale replay.
- File data is NOT crash-safe (data=writeback mode).

---

## 15. Design Decisions and Rationale

### Extents over indirect blocks

Traditional filesystems (ext2, FFS) use a tree of block pointers: direct, single-indirect, double-indirect, triple-indirect. For a 100 MB file stored contiguously, indirect blocks require storing 25,600 block pointers (100 * 1024^2 / 4096). The same file needs a single 12-byte extent entry in EFS.

Extents also simplify the driver: no need to allocate and maintain indirect pointer blocks, no special handling for block address depths, and no per-block pointer writes during sequential writes to large files.

The tradeoff is that highly fragmented files (many small non-contiguous regions) need more extent entries than a pointer tree would. But fragmentation is bad for performance regardless of the metadata representation, and the allocation strategy (Section 13) is designed to minimize it.

### Metadata-only journaling

A journal costs write bandwidth: every metadata block is written twice, once to the log and once to its home location. Journaling file data as well would double the cost on the hot path, for a guarantee EDOS does not need. Metadata-only journaling (`data=writeback`, as in ext3 and jbd2) buys the property that actually matters, which is that the filesystem is always structurally consistent after a crash, and leaves file contents to `fsync`.

The journal is mandatory rather than optional. An optional journal means two code paths, two sets of crash semantics, and a class of bug that only shows up on images formatted the other way. `INCOMPAT_JOURNAL` is set on every image `efs-mkfs` writes, and the kernel refuses to mount without it.

### 256-byte inodes

128-byte inodes (as in ext2) are tight. After the fixed fields (mode, uid, gid, link_count, size, blocks, flags, timestamps), there is little room for a useful extent tree or inline data buffer.

256-byte inodes provide 176 bytes for `data_area`, which fits:
- 13 inline extents (`(176 - 12) / 12 = 13`), enough for ~1.66 GB with 4K blocks
- 176 bytes of inline file data (configuration files, short scripts, small metadata files)

The `inode_size` field in the superblock allows a future version to use larger inodes (e.g., 512 bytes for extended attributes or more inline extents) without breaking the on-disk layout description.

### SSD-optimized design

All design choices assume NVMe or SATA SSD storage:

- No cylinder groups or track-based allocation (SSDs have no seek time)
- TRIM/discard support as a compatible feature flag
- Contiguous allocation aimed at reducing metadata overhead (fewer extents to write), not at seek minimization
- Block size defaults to 4096 bytes (x86-64 page size), which aligns well with NVMe sector sizes

Rotational-media optimizations (seek-time heuristics, interleaved cylinder group metadata, preallocation to avoid fragmentation across head seeks) are explicitly omitted.

### Block groups despite SSD

SSD locality is irrelevant, but block groups provide:

- **Bounded bitmaps:** With a 1 TB disk and 4K blocks, a single global bitmap would be 32 MB. Per-group bitmaps are one block each.
- **Bounded allocation scans:** Searching one group's bitmap is O(blocks_per_group / 8) bytes = one cache line to a few pages.
- **Parallelism:** Future drivers can allocate from different groups in parallel without a global lock.
- **Incremental fsck:** A repair tool can check one group at a time without loading the entire filesystem into memory.

### CRC32 over stronger hashes

The checksum requirement is detecting accidental corruption (bit flips from buggy memory, storage firmware errors, etc.), not cryptographic integrity. CRC32 is:

- Trivially implementable in no_std Rust without external dependencies
- Fast (hardware-accelerated on x86 via the `crc32` instruction)
- Well-understood: the polynomial, initial value, and final XOR are unambiguous
- Sufficient: it detects all single-bit errors, all burst errors shorter than 32 bits, and most longer burst errors

SHA-256 or xxHash would provide better collision resistance, but CRC32 is adequate and simpler for this use case.

### No copy-on-write

CoW filesystems (btrfs, ZFS, APFS) never overwrite live data in place. Writes go to new locations; the old location is freed when no snapshot references it. This enables snapshots, checksummed data blocks, and atomic multi-block updates.

The implementation complexity is substantial: reference counting every block, managing snapshot trees, garbage collecting stale blocks, and handling out-of-space conditions gracefully. EFS prioritizes simplicity and a correct basic implementation over these advanced features. CoW semantics could be added as a major version bump with a new `incompatible_features` bit.

### Inline data

Many real-world files are very small: configuration snippets, symlink targets, empty files, single-line scripts. Storing these in separate data blocks wastes at least one full block (4 KB) per file and requires an extra I/O to read.

Inline data (storing up to 176 bytes directly in the inode) eliminates the data block entirely for these files, saving both space and I/O. The implementation cost is a simple flag check and a size guard during reads and writes.

---

## Appendix: Numeric Constants Summary

```
EFS_MAGIC              = 0x45465321   // "EFS!" in ASCII
EFS_VERSION_1          = 1
EFS_ROOT_INO           = 1
EFS_EXTENT_MAGIC       = 0xEF10
EFS_DEFAULT_INODE_SIZE = 256
EFS_DEFAULT_BLOCK_SIZE = 4096
EFS_DEFAULT_BLOCK_SIZE_LOG2 = 12
EFS_MAX_FILENAME_LEN   = 255
EFS_MAX_INLINE_DATA    = 176          // data_area size in inode
EFS_INLINE_EXTENTS_MAX = 13          // (176 - 12) / 12

// Inode mode file type masks
S_IFREG = 0x8000
S_IFDIR = 0x4000
S_IFLNK = 0xA000
S_IFMT  = 0xF000                     // mask for extracting file type

// Inode flags
INODE_FLAG_INLINE_DATA = 0x00000001
INODE_FLAG_IMMUTABLE   = 0x00000002

// Directory entry file type values
FT_UNKNOWN  = 0
FT_REG_FILE = 1
FT_DIR      = 2
FT_SYMLINK  = 7

// Feature flags
COMPAT_DISCARD   = 0x0000000000000001
INCOMPAT_JOURNAL = 0x0000000000000001

// Journal
JOURNAL_MAGIC       = 0x21534A45      // disk bytes 45 4A 53 21, "EJS!"
JOURNAL_BLOCK_MAGIC = 0x21424A45      // disk bytes 45 4A 42 21, "EJB!"
DESC_FLAG_ESCAPED   = 0x00000001
```

Note the two magic conventions. `EFS_MAGIC` is the value `0x45465321`, which on
disk (little-endian) reads `21 53 46 45`. The journal magics are the reverse: the
literal is chosen so the bytes on disk spell the ASCII directly.

## Appendix: Structure Sizes Summary

| Structure | Size |
|---|---|
| Superblock (defined fields) | 256 bytes |
| Superblock (on-disk, padded) | block_size bytes |
| Block Group Descriptor | 64 bytes |
| Inode | 256 bytes |
| Extent Header | 12 bytes |
| Extent Entry (leaf) | 12 bytes |
| Extent Index Entry (internal) | 12 bytes |
| Directory Entry Header | 12 bytes |
| Directory Entry (variable) | 12 + name_len bytes, rounded up to 4-byte alignment |
| Journal Superblock | 64 bytes |
| Journal Block Header | 24 bytes |

## Appendix: Inode data_area Layout (depth=0, 4K blocks)

```
Bytes  0 ..  1   : extent header magic (0xEF10)
Bytes  2 ..  3   : entries (u16, number of valid extents)
Bytes  4 ..  5   : max_entries (u16, always 13 for inode root)
Bytes  6 ..  7   : depth (u16, 0 for leaf)
Bytes  8 .. 11   : reserved (u32, zero)
---- extent 0 ----
Bytes 12 .. 15   : logical_block (u32)
Bytes 16 .. 17   : length (u16, high bit reserved)
Bytes 18 .. 19   : start_hi (u16)
Bytes 20 .. 23   : start_lo (u32)
---- extent 1 ----
Bytes 24 .. 35   : (same layout)
...
---- extent 12 ---
Bytes 156 .. 167 : (same layout)
---- unused ------
Bytes 168 .. 175 : (unused, zero)
```
