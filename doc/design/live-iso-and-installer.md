# Live ISO and `edos-install`

Implemented. This describes the shipped design; where the implementation
departs from the original plan, the reason is given inline.

The goal is to be able to hand someone one file. Today the ISO is not a
distributable artifact: it carries Limine and the kernel and nothing else, and
the kernel then goes looking for a root filesystem that only exists inside a
second file. This spec turns the ISO into a live image that boots to the desktop
on its own, and then adds an installer that copies that live system onto a disk.

---

## Where we are

`edos-x86_64.iso` is built by `GNUmakefile:212-228` from an `iso_root` tree that
holds exactly three things: `boot/kernel`, `boot/limine/*`, and
`EFI/BOOT/BOOTX64.EFI`. There is no root filesystem on it.

`limine.conf` passes:

```
cmdline: root=UUID=87654321-4321-8765-cba9-987654321fed rootfstype=efs
```

`mount_system_fs` (`kernel/src/main.rs:322`) calls `fs::api::list_partitions()`,
matches that UUID against each partition's `unique_partition_guid`, and mounts
the match. That UUID only exists on `sata-disk.img`, a 5G qcow2 built by
`GNUmakefile:268-274`: `sgdisk` writes one GPT partition with
`--partition-guid=1:$(PARTITION_UUID)`, and the host-side `efs-mkfs` formats it
and populates it from `filesystem/` (34 MB today, nearly all of it
`filesystem/bin`).

So booting requires both files, and on real hardware it requires flashing a 5G
image to a second physical drive before the machine is usable. That is the
problem to fix.

Relevant existing pieces:

| Piece | Where | Note |
|---|---|---|
| `AsyncBlockDevice` trait | `kernel/src/drivers/block_io.rs:194` | `submit_read`, `submit_write`, `submit_flush`, optional `submit_read_batch` |
| Block device registry | `kernel/src/drivers/block_io.rs` | `register(device_id, Arc<dyn AsyncBlockDevice>)` / `lookup`; no enumeration |
| GPT parsing | `kernel/src/fs/gpt.rs` | device-agnostic: reads through `block_io::lookup` |
| EFS formatter | `tools/efs-mkfs`, `libs/efs-common` | plain Rust, host-side today |
| FAT32 driver with a write path | `kernel/src/fs/fat32/write.rs` | needed for the ESP |
| devfs registration | `kernel/src/fs/devfs/mod.rs:418` | `register_device(path, Arc<dyn DevFsDevice>)` |
| Block page cache | `kernel/src/fs/block_page_cache.rs` | keyed `(device_id, page_block_idx)`; no invalidate-device call |

Missing: any Limine module request, any RAM-backed block device, and any block
device node in devfs.

Two structural facts the phases below depend on, both easy to get wrong:

- **Partition discovery is AHCI-only.** `fs::init` (`kernel/src/fs/mod.rs:457`)
  builds its partition list from `ahci::api::list_devices()`, which returns
  `DETECTED_DEVICES`, not the `block_io` registry. Registering a device in
  `block_io` does *not* make it appear in `list_partitions()`. USB storage works
  around this by calling `fs::api::register_partition` by hand
  (`kernel/src/drivers/usb/xhci/mod.rs:1364-1384`).
- **The partition list is built once and never refreshed.** `FsRequest::Mount`
  resolves `(device_id, partition_index)` against that one vector, and the only
  way to extend it is the kernel-internal `register_partition`; no syscall
  reaches it. So a userspace program cannot mount a partition it just created.

---

## Phase 1 — the live ISO

**Outcome.** `make iso` produces one file that boots on any UEFI machine
straight to the desktop, with a writable root that lives in RAM and is discarded
on reboot.

### The shape of it

Build the live root as **a complete GPT disk image**, not a bare filesystem, and
carry it as a Limine module. Keeping the partition table means the `root=UUID=`
match, `parse_gpt` and `mount_partition` all keep working unchanged, because
they address a device by id and read through `block_io::lookup`. This is the
whole reason to prefer it over a raw EFS blob plus a new whole-device mount
path.

What does *not* come for free is discovery: the ramdisk has to be enumerated
into the partition list, which today only happens for AHCI devices. See 1.4.

Reuse `PARTITION_UUID`, so the same `limine.conf` cmdline selects either the RAM
disk or a real SATA disk depending on which is present. When both are present
the SATA one should win, because that is the installed system; see "Root
selection" below.

### 1.1 Build the image

New makefile target, modelled on the `sata-disk.img` recipe but sized to the
payload rather than 5G:

```make
live-root.img: $(FILESYSTEM_FILES) tools/efs-mkfs/src/*.rs libs/efs-common/src/*.rs
	# size = populated tree + slack, rounded up; keep it honest, it is RAM
	qemu-img create -f raw live-root.img $(LIVE_ROOT_SIZE)
	sgdisk live-root.img -n 1:2048 -t 1:0700 -c 1:"EDOS_DATA" \
		--partition-guid=1:$(PARTITION_UUID)
	cargo build --release --manifest-path tools/efs-mkfs/Cargo.toml
	tools/efs-mkfs/target/release/efs-mkfs --partition-offset 1048576 \
		--populate filesystem/ --label EDOS live-root.img
```

The `cargo build` line is not optional; the `sata-disk.img` recipe has it, and
without it the target fails on a clean tree.

`LIVE_ROOT_SIZE` should be computed from `du -sb filesystem` plus headroom (say
+40%, minimum 64 MiB), not hard-coded, so it cannot silently overflow when
userspace grows. Print the chosen size.

Then add it to the ISO tree and reference it from `limine.conf`:

```
    module_path: boot():/boot/live-root.img
```

ISO grows by the image size. With the tree at 34 MB that is roughly a 90 MB ISO.
If that becomes uncomfortable, the honest fix is to stop shipping debug binaries
in `filesystem/bin`, not to compress the module.

### 1.2 Take the module from Limine

The `limine` crate (0.6.5, already a dependency) exposes `ModulesRequest`, not
`ModuleRequest`. Add it next to the other requests in `kernel/src/boot.rs`, with
`#[used]` and `#[unsafe(link_section = ".requests")]` like its neighbours, and
surface the module through `BootInfo`.

Three things to get right:

- `ModulesResponse::modules()` returns `&[&File]`, so `File::data_mut()`, which
  takes `&mut self`, is unreachable from it. Take `data()`'s pointer and length
  and build the `&'static mut [u8]` yourself. That is the one unsafe line in the
  ramdisk; keep it there rather than spreading raw pointers into the driver.
- The addresses are **already HHDM virtual**. Limine base revision 4 and up
  returns virtual pointers in responses, which is why `boot.rs:146` subtracts
  `physical_memory_offset` back out for the RSDP. Do not convert the module
  address; use it as-is.
- Log the module's base, length and the resulting device id at init, in the same
  style as the other drivers. This is the first thing anyone will want when it
  does not mount.

The frame allocator needs no work here. `mark_non_usable_frames`
(`kernel/src/memory/frame_allocator.rs:194`) fills the whole bitmap with `0xFF`
and then frees only `MEMMAP_USABLE` regions, and nothing in the tree ever
reclaims bootloader-reclaimable memory, so the module's frames stay allocated
for the life of the boot. Add a debug assert at ramdisk init that the module's
first and last frame are marked allocated, so that stays true if the allocator's
memory-map handling is ever widened.

### 1.3 `RamBlockDevice`

New file, `kernel/src/drivers/ramdisk.rs`. Implements `AsyncBlockDevice` over a
`&'static mut [u8]`:

- 512-byte sectors, to match the rest of the system.
- `submit_read` / `submit_write` copy and complete **synchronously**, returning
  an already-completed `BlockIoHandle`. Nothing parks. Check how
  `BlockIoHandle` signals completion for the AHCI fast path and reuse that; do
  not invent a second completion protocol.
- `submit_flush` is a no-op that completes.
- Bounds-check every request against the image length and return
  `BlockError::…` rather than panicking. A bad LBA must not take the kernel down.
- Writes go **in place**, into the module's own memory. That is what makes the
  live system writable, costs no extra RAM, and loses everything on reboot,
  which is exactly live-CD semantics. Say so in the module doc comment.

Register it in `block_io` from `drivers::init_drivers()`, so it exists before
`fs::init` scans, under a device id that cannot collide with AHCI's or with USB
storage's `1000 + idx`. Use `2000`.

### 1.4 Make the ramdisk discoverable

Registering in `block_io` is not enough: `fs::init` iterates
`ahci::api::list_devices()`, so the ramdisk would never be scanned for a GPT.

Fix it in the registry rather than by adding a third special case. Add
`block_io::list() -> Vec<u64>` (the registry already holds every device; it just
does not expose the keys) and have `fs::init` iterate that instead of the AHCI
list. The ATAPI skip stays, but becomes an AHCI-specific filter applied to ids
AHCI owns rather than the shape of the loop.

This subsumes the USB workaround: once discovery is registry-wide, USB storage
can drop its hand-rolled `register_partition` call and be found by the same
scan. Do that in the same change, or the tree keeps two answers to "how does a
partition get into the list".

`register_partition` stays, because USB devices appear after boot and still need
to push into an already-built list.

One detail that bites at mount time: `mount_system_fs` does
`part.filesystem.as_ref().expect("expected fs type")`, so any partition reaching
it must have run filesystem detection. Make sure the ramdisk's entry goes
through the same detection path the AHCI entries do, rather than being
constructed with `filesystem: None`.

### 1.5 Root selection

`mount_system_fs` (`kernel/src/main.rs:331`) does not just take the first
matching partition; when *nothing* matches it leaves `part_idx` at its initial
`0` and mounts `partitions[0]` regardless. That is already wrong, and a second
GPT device makes it dangerous: a stale or mistyped UUID silently mounts whatever
enumerated first, which on an installed machine can be the ESP.

Fix the selection to be explicit rather than positional:

- Make the match an `Option<usize>`. No match is a distinct outcome, never
  index 0.
- Prefer a match on a non-ramdisk device, so an installed disk wins over the
  live image that booted it. The ramdisk is identified by its device id, which
  the `Partition` already carries.
- Accept `root=live` on the cmdline to force the ramdisk, for testing and for a
  "boot the live system anyway" Limine entry.
- If nothing matches, log every partition it did see, with device id, index,
  GUID and detected filesystem, then fall back to memfs. The current failure
  mode tells you nothing.

### 1.6 Acceptance

- `make iso` from a clean tree produces one file. `qemu-system-x86_64` with only
  that ISO attached (no `-drive`) boots to the desktop with a shell prompt.
- `touch /home/x && ls /home` works; after a reboot the file is gone.
- `df` shows the root filesystem on the ramdisk device.
- `dd if=edos-x86_64.iso of=/dev/sdX` onto a USB stick boots a real UEFI machine
  to the desktop.
- With `sata-disk.img` also attached, the SATA root is chosen, and `root=live`
  overrides that.
- Booting with a deliberately wrong `root=UUID=` mounts nothing and says so,
  instead of mounting the first partition it found.

---

## Phase 2 — `edos-install`

**Outcome.** From the live system, `edos-install /dev/sda` produces a machine
that boots from its own disk with a persistent filesystem.

This phase is about giving userspace the three things it currently lacks: raw
access to a disk, the ability to create filesystems, and a way to tell the
kernel that a disk's partition table has changed.

### 2.1 A block device node in devfs

devfs registers `/fb`, `/tty0`, `/random`, `/dsp`, `/kbd`, `/mouse` and `/klog`
today; there is no way to reach a disk from userspace at all.

Add a `DevFsDevice` that wraps a registered `AsyncBlockDevice` and exposes
byte-addressed `read`, `write` and seek at 512-byte granularity, registered as
`/dev/sda`, `/dev/sdb`, … in discovery order, one node per **device**, not per
partition. Partition nodes are not needed: the installer writes the partition
table itself and can compute offsets.

Guard rails, because this is a foot-gun by construction:

- Reject unaligned offsets and lengths rather than silently rounding.
- Refuse writes to the device currently backing a mounted filesystem unless the
  caller passes an explicit override flag. Getting this wrong corrupts the
  running system.

**Cache policy: go through the block page cache.** This is decided, not left
open. The cache is keyed `(device_id, page_block_idx)`
(`kernel/src/fs/block_page_cache.rs:72`), and the installer writes to the same
device id that the filesystem driver will read through moments later when the
new partition is mounted. A device node that bypassed the cache would leave
stale pages under that key and produce exactly the "the install looked fine but
does not boot" failure. So:

- `write` does `read_page` + memcpy + `mark_dirty`, `read` does `read_page`,
  both at 512-byte granularity inside the page.
- The node's `ioctl` exposes a flush that writes back every dirty page for the
  device and waits, so `edos-install` can fsync before it reports success.

`BlockPageCache::invalidate_device(device_id)` already existed and is called
from the rescan in 2.2. It gained one thing: pages still dirty after its flush
are written out before eviction rather than dropped.

Access is byte-granular, not sector-granular as first planned. The cache reads
a page before patching it, so a short write updates exactly the bytes given;
rejecting unaligned access would have bought nothing and would have forced a
second, sector-aligning implementation of the formatter.

### 2.2 A partition-rescan syscall

Without this the installer cannot mount anything it creates, and steps 2.4 and
2.5 have no way to run.

`SYS_MOUNT` (`kernel/src/syscalls/mod.rs:334`) names a partition by
`(device_id, partition_index)`, and the FS thread resolves that against the
partition list it built once at `fs::init`. A partition table written through
`/dev/sda` a second ago is not in that list, so the mount fails with no way for
userspace to fix it.

Implemented as an ioctl on the device node rather than a new syscall number:
`BLOCK_IOCTL_RESCAN` on `/dev/sd*`, backed by a new
`FsRequest::RescanPartitions`. Same mechanism, scoped to the device the caller
already opened. The node also carries `BLOCK_IOCTL_FLUSH`,
`BLOCK_IOCTL_SECTOR_COUNT` and `BLOCK_IOCTL_IS_MOUNTED`. On the FS thread:

1. Refuse if any mount is backed by that device id; re-partitioning under a live
   mount is not something to make recoverable.
2. `BlockPageCache::invalidate_device(device_id)`.
3. Drop every partition with that device id from the list.
4. Re-run `parse_gpt`, falling back to `parse_mbr` exactly as `fs::init` does,
   and push the results.

Extract that sequence as one function and have `fs::init` call it per device, so
boot-time discovery and rescan cannot drift apart.

Return the number of partitions found, so `edos-install` can check it got the
two it wrote rather than mounting by index and hoping.

### 2.3 `efs-mkfs` as a userspace program

`tools/efs-mkfs` and `libs/efs-common` are ordinary Rust with no host-specific
dependencies beyond `std`, and EDOS userspace links a real `std`. Add a
`programs/efs-mkfs` member that reuses `libs/efs-common` and writes through the
new device node.

Keep one implementation: the host tool and the guest tool should differ only in
their `main`, so a format written on the host and one written in the guest
cannot drift.

### 2.4 GPT and ESP creation

- **GPT writing.** `kernel/src/fs/gpt.rs` only reads. Write a userspace GPT
  writer: protective MBR, primary and backup headers, partition array, CRC32s.
  Two partitions: an ESP (type `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`, 512 MiB)
  and the EDOS root (type `0700`, rest of the disk) with a **freshly generated**
  partition GUID. Do not reuse the hard-coded `PARTITION_UUID`; two installed
  disks in one machine would then be indistinguishable.
- **FAT32 formatting.** Write a minimal `mkfs.fat` in the same program: BPB,
  FSInfo, backup boot sector, two FATs, an empty root cluster. This is well
  specified and small; the kernel already reads FAT32, so a bad format shows up
  immediately as a failed mount rather than silently.
- **Populating the ESP.** Rescan (2.2), then mount the new ESP through the
  kernel's FAT32 driver (`kernel/src/fs/fat32/write.rs` has the write path) and
  copy in
  `EFI/BOOT/BOOTX64.EFI`, `boot/kernel` and a generated `boot/limine/limine.conf`
  whose `cmdline` carries the new root partition's GUID. The live ISO must
  therefore carry a copy of those three files inside the live root, not only in
  the ISO's own filesystem, since the ISO is not mounted at that point.

### 2.5 The program

`programs/edos-install`:

```
edos-install [--esp-size 512M] [--yes] <device>
```

1. Refuse to run if the target device backs a mounted filesystem.
2. Print the plan — device, size, the two partitions, and that everything on the
   device will be destroyed — and require confirmation unless `--yes`.
3. Write GPT, format the ESP, format the root.
4. Rescan partitions, and check the count matches the two it wrote.
5. Mount both, copy the live root across, copy the boot files into the ESP,
   write `limine.conf` with the new GUID.
6. Flush the device node, unmount both, and report what to do next.

Every step logs what it is about to do before it does it, so a failed install
can be read off the serial log.

### 2.6 Acceptance

- On a QEMU machine booted from the live ISO with a blank disk attached:
  `edos-install /dev/sda`, then reboot with the ISO detached, and the machine
  boots from the disk into the desktop.
- A file written before the reboot is still there afterwards.
- `efs-fsck` on the host reports the installed root clean.
- Running it against the device that backs the running root is refused.
- An interrupted install (kill it mid-copy) leaves a disk that fails to mount
  cleanly rather than one that mounts and returns garbage.

---

## Order and risk

Phase 1 is self-contained and worth shipping on its own: it is what makes the
project downloadable. Phase 2 depends on it only for the live environment to run
in.

The risky parts, in order:

1. **Widening partition discovery to the whole block registry** (1.4). It moves
   boot-path code that currently works, and folding the USB special case into it
   means a mistake shows up as "USB storage stopped mounting", not as anything
   to do with the live ISO. Boot with the USB disk (`make run-storage`) before
   and after.
2. **Cache coherence between raw device writes and a later mount** (2.1, 2.2).
   The policy is decided; the risk is the missing `invalidate_device` and
   getting the rescan ordering wrong. Test a second install onto a disk already
   mounted once in the same boot.
3. **FAT32 formatting.** Easy to get subtly wrong; firmware is unforgiving. Test
   the generated ESP by mounting it on the host with `mtools` as well as in the
   guest.

Not a risk, contrary to what it looks like: the frame allocator does not reclaim
the module. It frees only `MEMMAP_USABLE` and never touches bootloader-
reclaimable memory. The debug assert in 1.2 is there to keep that true, not
because it is currently in doubt.

## Documentation to update in the same commits

- `README.md` — the "On real hardware" section currently tells people to flash
  `sata-disk.img` to a spare drive.
- `CLAUDE.md` — the boot-flow paragraph describes the root partition being
  selected from the Limine cmdline UUID only.
- `doc/vm-control.md` — host requirements mention needing `sata-disk.img`.
- `ideas.txt` — close the entry when the phase lands. The "In-EDOS fsck" entry
  also carries a prereq investigation asking whether `/dev/sda` exists; 2.1
  answers it, so rewrite that paragraph in the same commit.
- `doc/invariants/lock-order.md` — only if `invalidate_device` needs a lock the
  block page cache does not already take. It should not; write it in terms of
  the existing shard locks and the rule that they are never held across I/O.
- The website: `edos.edgl.dev` downloads page ships whatever the release
  contains, so a live ISO replaces the two-file instructions there.
