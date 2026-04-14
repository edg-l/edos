# PageCacheOps for FAT32 and memfs: Design Notes

## 1. Inode number encoding

For FAT32, the inode number (ino) is encoded as a dirent position:

```
ino = ((dir_cluster as u64) << 32) | (entry_offset as u64)
```

Where `dir_cluster` is the cluster that contains the directory entry (>= 2 for all
FAT32 cluster-based directories) and `entry_offset` is the byte offset of the short
DirectoryEntry within that cluster's data.

For FAT12/16, the legacy encoding (first_cluster as u64) is preserved; PageCacheOps
is not supported for those variants.

Root directory ino for FAT32 remains `boot_info.root_cluster as u64` (unchanged),
since `find_dir_entry` returns None for the root path (no parent entry).

Helper functions:
- `fat_ino_from_pos(dir_cluster: u32, entry_offset: usize) -> u64`
- `split_fat_ino(ino: u64) -> (u32, u32)`

## 2. FatInodeEntry schema

```rust
#[derive(Debug, Clone)]
pub struct FatInodeEntry {
    pub dir_cluster: u32,     // cluster holding the dirent
    pub entry_offset: u32,    // byte offset of short dirent within that cluster
    pub first_cluster: u32,   // head cluster of the file's data chain (0 = no data)
    pub file_size: u32,       // current file size in bytes
    pub mappers_pin: u32,     // count of live FileBacked VMAs via Fatfs::mmap
    pub orphan: bool,         // true after remove_file while mappers_pin > 0
}
```

The side table is `Fatfs.inode_table: BlockingMutex<BTreeMap<u64, FatInodeEntry>>`.

## 3. Lifecycle diagram

```
lookup / resolve_inode
  -> find_dir_entry -> fat_ino_from_pos(dc, eo)
  -> upsert inode_table[ino] (preserves mappers_pin/orphan if exists)

create_file
  -> append_dir_entry returns (cluster, offset)
  -> insert inode_table[ino] with first_cluster=0, file_size=0, mappers_pin=0, orphan=false

write_bytes / page_cache write via flush_page
  -> after write succeeds, update inode_table[ino].first_cluster and .file_size

truncate
  -> update inode_table[ino].first_cluster (if size==0, set to 0), .file_size

remove_file
  -> look up inode_table[ino]
  -> if mappers_pin == 0: free chain, remove inode_table entry, mark dirent 0xE5
  -> if mappers_pin > 0: mark orphan=true, mark dirent 0xE5, keep inode_table entry

mmap (Fatfs::mmap override)
  -> pin_inode(ino): increment inode_table[ino].mappers_pin

unmap / exit (on_unpin hook at two sites)
  -> unpin_inode(ino): decrement mappers_pin
  -> if mappers_pin == 0 && orphan == true: free cluster chain, remove inode_table entry

fill_page
  -> lookup_inode_entry(ino) -> cluster_at_index(entry.first_cluster, page_index)
  -> direct::read_sectors from cluster LBA, copy into buf

flush_page
  -> lookup_inode_entry(ino) -> ensure_chain_to / cluster_at_index
  -> compute valid bytes from side table (NOT from valid_bytes arg)
  -> direct::write_sectors to cluster LBA

update_size (grow-only)
  -> if new_size > u32::MAX: return Error::IoError
  -> if new_size <= entry.file_size: return Ok(()) (no shrink)
  -> patch dirent (skip if orphan), update inode_table[ino].file_size
```

## 4. Pin-count contract and unpin hook locations

The pin counter `mappers_pin` is updated:
- Incremented at: `Fatfs::mmap` override (after successful mmap setup)
- Decremented at two unpin sites:

Site 1: `kernel/src/syscalls/memory.rs` - `sys_munmap` function
  - After the `VmaBacking::FileBacked` PTE unmap + `drop(pages)` at line ~672
  - Call `vfs::on_unpin(inode.mount_id, inode.ino)`

Site 2: `kernel/src/thread/thread.rs` - exit VMA teardown loop
  - After `VmaBacking::FileBacked` arm at line ~854 (after `fa.dec_refcount`)
  - Call `vfs::on_unpin(inode.mount_id, inode.ino)` (use inode.mount_id and inode.ino
    before the Arc is dropped with the VMA)

The `on_unpin` hook is a new `FileSystem` trait method (default no-op) overridden by
`Fatfs` to call `self.unpin_inode(ino)`. Exposed via `vfs::on_unpin(mount_id, ino)`.

## 5. FAT12/16 fallback note

`resolve_inode`, `create_file`, `write_bytes`, `truncate`, `remove_file` all gate
inode_table operations on `FatVariant::Fat32`. FAT12/16 paths continue to use
first_cluster as ino and do not populate the side table.

`as_page_cache_ops` returns `None` for FAT12/16 and `Some(self)` for FAT32 only.
This ensures the VFS falls back to `read_bytes`/`write_bytes` for FAT12/16 mounts.

## 6. Known coherency gaps (v1)

- `cluster_at_index` and `ensure_chain_to` both walk the cluster chain from the head
  on every call. Sequential fill_page/flush_page over a large file is O(N^2) in
  cluster count. Acceptable for v1 given typical file sizes.

- MAP_SHARED coherency with concurrent path-based write_bytes on the same FAT32 file:
  flush_page overwrites the cluster data from the cached frame. If a concurrent
  write_bytes call is in progress, the result is last-writer-wins at the sector level.
  This mirrors v1 design and will be revisited if write contention is observed.

## 7. FAT32 mount status

FAT32 is NOT auto-mounted at boot. The kernel boot sequence in `kernel/src/main.rs`
only auto-mounts:
- Root EFS partition (or memfs fallback)
- devfs at `/dev`
- procfs at `/proc`
- memfs at `/tmp`

FAT32 must be explicitly mounted by userspace via the `mount` syscall:
```
mount <device_id> <partition_index> /mnt/fat fat32
```

Phase 5 test 9 requires either:
  (a) A shell script that mounts the FAT32 partition before running mmaptest, or
  (b) An mmaptest harness that performs the mount syscall itself.

The mountpoint is conventionally `/mnt/fat` but is not fixed by the kernel.
Task 5.2 should confirm the actual mountpoint with the user before hardcoding it.
