# efs-fsck: EFS Filesystem Checker

`efs-fsck` is a host-side tool that validates and optionally repairs an EFS
filesystem image or raw block device. It is the v1 checker: journal replay,
inode scan, bitmap rebuild and leak reclamation, directory-tree reachability,
and superblock/BGD cross-check.

See also: `doc/efs.md` for the on-disk format specification.

---

## Invocation

```
efs-fsck [OPTIONS] <IMAGE>
```

`<IMAGE>` may be a raw image file, a block device, or a qcow2 image. qcow2 is
decoded read-only: `--repair` on one is refused, since writing it means
allocating clusters and maintaining refcounts. A qcow2 image with a backing
file, encryption, compressed clusters or any incompatible feature bit is
rejected rather than half-read.

### Options

| Flag | Description |
|------|-------------|
| `--repair` | Enable destructive fixes. Without this flag fsck is read-only. |
| `--yes` / `-y` | Auto-accept all repair prompts without interactive confirmation. |
| `--dry-run` / `-n` | Show what would be repaired without writing anything. |
| `--verbose` / `-v` | Print `INFO`-level findings (scan stats, replay counts, etc.). |
| `--force` | Override safety checks: skip the `fsck_in_progress` sentinel and skip journal replay if the JSB is unreadable. |
| `--partition-offset N` | Byte offset of the EFS partition within the image (e.g. `1048576` for a GPT image with a 1 MiB gap). Defaults to 0. |
| `--help` | Print help and exit. |

### Running on a partition image

```
# Raw image, no offset
efs-fsck sata-disk.raw

# GPT image: EFS partition starts at 1 MiB
efs-fsck --partition-offset 1048576 sata-disk.raw

# Block device
efs-fsck /dev/sdb1
```

### Checking the development disk

`sata-disk.img` is checked in place. Its EFS partition is the one GPT partition
`make sata-disk.img` creates, at the usual 1 MiB gap:

```
efs-fsck --partition-offset 1048576 sata-disk.img
```

Repairing it still needs a raw copy:

```
qemu-img convert -O raw sata-disk.img /tmp/sata.raw
efs-fsck --repair --partition-offset 1048576 /tmp/sata.raw
```

Stop the VM first: a check of a disk a running guest is still writing reports
findings that are only in-flight state. The conversion is a copy, so `--repair`
on it fixes nothing on the original.

---

## Exit Codes

`efs-fsck` follows the Linux fsck exit code convention.

| Code | Name | Meaning |
|------|------|---------|
| 0 | Clean | No errors found, or all errors fixed. |
| 1 | ErrorsFixed | Errors were found and successfully repaired (`--repair` was active). |
| 4 | ErrorsRemain | Errors were found and either `--repair` was not given, or some fixes were declined or failed. |
| 8 | OperationalError | fsck could not run: I/O error, bad magic, unreadable superblock, or `fsck_in_progress` sentinel set without `--force`. |
| 16 | UsageError | Bad command-line arguments. |

Exit codes may be combined (OR'd) in future versions for tools that run fsck on
multiple filesystems, but `efs-fsck` v1 returns exactly one code per run.

---

## Finding Categories

Each finding is tagged with a severity and a category. Severity levels:

- `INFO`: informational, only printed with `--verbose`.
- `WARN`: potential problem, does not affect the exit code.
- `ERROR`: confirmed problem, triggers exit 4 if unrepaired.

### Journal (`journal`)

Checks the Journal Superblock (JSB) at `journal_first_block`, then the
transaction ring.

| Finding | Severity | Fixable | What fsck does |
|---------|----------|---------|----------------|
| `journal is dirty` (tail_seq != head_seq) | ERROR | yes | With `--repair`: replays committed transactions to home locations, resets JSB (`tail = head`). Without `--repair`: reports and exits 4. |
| JSB magic/version mismatch | ERROR | no | Refuses replay; suggests `--force` to skip. |
| JSB CRC mismatch | ERROR | no | Same as above. |
| JSB `block_size` != FS block_size | ERROR | no | Refuses replay. |
| Commit block CRC mismatch mid-ring | INFO | no | Stops replay at that point; treats subsequent ring content as uncommitted. This is normal for a torn write. |

### Superblock (`superblock`)

| Finding | Severity | Fixable | What fsck does |
|---------|----------|---------|----------------|
| Bad magic / unsupported version | FATAL | no | Exits with OperationalError immediately. |
| CRC mismatch | WARN | yes | With `--repair`: rewrites primary SB after all other fixes. |
| Unknown incompatible feature bits | ERROR | no | Reports only; cannot safely proceed. |
| `fsck_in_progress` sentinel set | FATAL | no | Exits with OperationalError unless `--force`. |
| Total blocks * block_size > device size | ERROR | no | Reports only. |

### Block Group Descriptors (`bgd`)

| Finding | Severity | Fixable | What fsck does |
|---------|----------|---------|----------------|
| BGD checksum mismatch | ERROR | yes | With `--repair`: recomputes all BGD checksums after bitmap fixes. |
| Sum of BGD `free_blocks_count` != `sb.free_blocks` | ERROR | yes | Same repair as above. |
| Sum of BGD `free_inodes_count` != `sb.free_inodes` | ERROR | yes | Same repair as above. |

### Block Bitmap (`block-bitmap`)

| Finding | Severity | Fixable | What fsck does |
|---------|----------|---------|----------------|
| `leaked block-bitmap bit at X` | ERROR | yes | With `--repair`: clears the bit in the on-disk bitmap. Increments BGD `free_blocks_count`. |
| `missing bit in block-bitmap for index X` | ERROR | no | Reports only. A block appears referenced but is marked free; root-cause analysis is needed. |
| `block X double-claimed by inodes A and B` | ERROR | no | Reports only. Manual resolution required. |

### Inode Bitmap (`inode-bitmap`)

| Finding | Severity | Fixable | What fsck does |
|---------|----------|---------|----------------|
| `leaked inode-bitmap bit at X` | ERROR | yes | With `--repair`: clears the bit in the on-disk inode bitmap. Increments BGD `free_inodes_count`. |
| `missing bit in inode-bitmap for index X` | ERROR | no | Reports only. |

### Inode (`inode`)

| Finding | Severity | Fixable | What fsck does |
|---------|----------|---------|----------------|
| Inode CRC mismatch | WARN | no | Reports only. |
| `link_count == 0` but inode bitmap says allocated | ERROR | no | Reports only; treated as structural corruption. |
| Inline-data inode `size > INODE_DATA_AREA_SIZE` | ERROR | no | Reports only. |
| Extent header magic mismatch | ERROR | no | Reports only. |
| Extent `depth > 1` | ERROR | no | Reports only (v1 supports depth 0 and 1). |
| Extent index child block outside `1..total_blocks` | ERROR | no | Reports only; inode skipped. |
| Extent leaf block malformed (magic, `depth != 0`, entry overflow) | ERROR | no | Reports only; inode skipped. |
| Extent `physical_start + length > total_blocks` | ERROR | no | Reports only; extent not applied to rebuilt bitmap. |
| Extent `length == 0` | ERROR | no | Reports only. |

### Directory Tree (`dir-tree`)

| Finding | Severity | Fixable | What fsck does |
|---------|----------|---------|----------------|
| `inode N link_count on-disk=X observed=Y` | ERROR | yes | With `--repair`: updates `inode.link_count` to the observed value, recomputes inode CRC. |
| `orphan inode N (reachable link count 0)` | ERROR | yes | With `--repair` (separate prompt, default NO): clears inode bitmap bit, zeros inode table entry, frees extent blocks. **Destructive.** |
| `.` entry points to wrong inode | ERROR | no | Reports only. |
| `..` entry points to wrong parent | ERROR | no | Reports only. |
| Directory cycle detected | ERROR | no | Reports only; that subtree is not descended into. |
| Dir entry points to out-of-range inode | ERROR | no | Reports only. |
| Dir entry points to unallocated inode | ERROR | no | Reports only. |
| Malformed dir entry (`rec_len`, alignment) | ERROR | no | Reports only; parsing of that block stops. |

---

## What `--repair` Will and Will Not Fix

### Will fix (with appropriate confirmation)

- Journal replay: uncommitted transactions written to home locations; JSB reset.
- Leaked block bitmap bits: clear the bit, increment BGD free count.
- Leaked inode bitmap bits: clear the bit, increment BGD free count.
- BGD checksum mismatches and free-count sum discrepancies.
- Inode `link_count` mismatches: update inode to match observed count.
- Orphan inodes (if accepted): free extent blocks, clear inode bitmap, zero inode.
- Backup superblock discrepancies: overwrite from primary after all other fixes.

### Will NOT fix

- Double-claimed blocks: two inodes reference the same extent block. fsck
  reports both inodes; the user must decide which is canonical.
- Broken `.` or `..` entries: directory structure corruption is too risky to
  auto-repair without knowing the intended parent.
- Directory cycles: not auto-resolvable.
- Malformed directory entries (`rec_len` violations, truncated headers).
- Missing bitmap bits (`rebuilt=1, on-disk=0`): implies a reference to a block
  the FS believed was free. Deeper analysis required.
- Extent trees with `depth > 1`: fsck walks depth-1 index nodes but not deeper ones.
- Directory block corruption: fsck never rewrites directory blocks in v1.

---

## The `fsck_in_progress` Sentinel

When `--repair` is active, fsck sets the `fsck_in_progress` byte in the primary
superblock (and all backup superblocks) before writing any repairs, and clears
it on clean exit.

If fsck is killed mid-repair (SIGKILL, power loss, panic), the sentinel remains
set. The next invocation refuses to run and prints:

```
error: previous fsck did not complete cleanly (fsck_in_progress sentinel set).
       Run with --force to override.
```

`--force` clears the sentinel and continues. After a crash, it is advisable to
run `efs-fsck --force --repair --yes` and then a second read-only pass to
confirm the filesystem is clean.

---

## Limitations (v1)

- **Extent tree depth > 1**: EFS emits depth-0 and depth-1 trees, and fsck
  walks both — a depth-1 inode's leaf blocks are validated and seeded into the
  rebuilt block bitmap alongside its data blocks. A `depth > 1` header is
  reported as an error and that inode's blocks are skipped.
- **No `lost+found`**: orphan inodes are offered for deletion, not recovery.
  A `lost+found` directory is a v2 feature.
- **No FAT32 or memfs**: `efs-fsck` only handles the EFS (`EFS!` magic) format.
  The kernel's other filesystem drivers (FAT32, memfs, procfs, devfs) are not
  supported.
- **No online fsck**: running against a mounted block device is unsafe. On
  Linux, fsck attempts an exclusive `flock`; if the device is locked, it exits
  with OperationalError unless `--force`.
- **Large images beyond available RAM**: the in-memory bitmap and link-count map
  are proportional to total_blocks and total_inodes respectively. For very large
  images (hundreds of GiB) this may exceed available RAM; there is no streaming
  mode in v1.
- **Cluster size != block size**: not supported by mkfs or fsck in v1.
- **Partial journal-commit testing**: fsck can detect a corrupt JSB checksum but
  cannot test mid-ring commit corruption without a kernel-generated image
  containing real transactions (the test `partial_tx_discarded` is `#[ignore]`
  for this reason).

---

## Cross-references

- `doc/efs.md`: on-disk format specification (superblock, BGD, inode, extents,
  directory entries, journal).
- `tools/efs-mkfs/`: filesystem formatter that creates images compatible with
  this checker.
- `libs/efs-common/`: shared on-disk struct definitions used by both mkfs and
  fsck.
- `kernel/src/fs/journal/`: kernel-side journal implementation, replay logic
  is ported to `tools/efs-fsck/src/replay.rs`.
