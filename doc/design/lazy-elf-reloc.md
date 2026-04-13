# Lazy ELF Relocation

## 1. Problem

Eager `R_X86_64_RELATIVE` patching at load time cost ~200 ms per binary
(edos-wm has ~14 000 entries). Each entry required: resolve the target page
via the page cache, allocate a private frame, copy the cache page, apply the
patch, and finally tighten PTE permissions. Under a cold cache this triggered
hundreds of single-page AHCI reads. Cold-spawn of `/bin/sh` from the terminal
measured 238 ms before this work.

## 2. Design

A `RelocTable` (see `kernel/src/loader/reloc.rs`) is parsed at load time from
all SHT_RELA sections of the binary. It stores `R_X86_64_RELATIVE` entries
sorted by offset and indexed into per-page buckets for O(1) fault-path lookup.
The table is wrapped in `Arc<RelocTable>` and stored on the process alongside
`LoadedInfo`.

Reloc-target pages are mapped via `VmaBacking::FileBacked` with the
`LAZY` flag, identical to all other file-backed PT_LOAD pages. On the first
write fault to a reloc-target page the fault handler:

1. Allocates a fresh private frame.
2. Copies the cache page into the private frame via HHDM.
3. Calls `RelocTable::apply_relocs_to_page` to patch every RELATIVE entry
   whose target lands in this page.
4. Maps the frame writable (no COW_BIT, this page is always private).

The shared cache page is never modified. `.text` and `.rodata` pages continue
to share physical frames across all processes.

## 3. Always-private invariant for reloc-target pages

Every `R_X86_64_RELATIVE` target in current EDOS binaries lands inside a
single writable PT_LOAD segment (Phase 0 census confirmed this; the loader
asserts it). Writable PT_LOAD pages are already private-on-fault (write fault
allocates a private frame copied from cache). The reloc path reuses this
mechanism: it fires from the same fault handler path, just with an extra
`apply_relocs_to_page` call before the mapping is made writable.

Read-only pages (.text, .rodata) are unaffected: they have no reloc targets
and continue to share the cache frame.

## 4. Partial-last-file-page caveat

When `p_filesz` is not a multiple of 4096, the last file page must have bytes
`[p_filesz % 4096 .. 4096]` zeroed per the ELF spec (these bytes are the BSS
portion that shares the page). The page cache holds whatever the linker left
on disk.

`prefault_elf_tail_page_from_cache` pre-faults this page at load time:
allocates a private frame, copies the cache page, zeroes the tail, and maps
the page writable. Because the page is already mapped, the lazy fault path
will never fire for it. Any RELATIVE entries targeting this page are applied
immediately by walking `reloc_pages` after the `RelocTable` is built. This
was introduced in commit `c53914d`.

## 5. Fork

`Arc<RelocTable>` is cloned in `sys_fork` (`syscalls/mod.rs`, the fork path).
Pages already faulted by the parent are private writable frames; the fork's
COW handler treats them normally (BIT_9 COW marker on read-only shared
frames, immediate copy on write). Pages not yet faulted remain file-backed
LAZY and the child's fault handler applies relocs independently.

## 6. Truncate invalidation

Reloc-target pages are private frames from first fault. They are never
registered as `FileBacked` PTEs in the MM's dirty/reverse-map tracking (the
fault handler maps them as anonymous after copying). The truncate invalidator's
reverse-map walk only visits `FileBacked` PTEs, so reloc-target pages are
skipped naturally without any special casing.

## 7. Limitations

Only `R_X86_64_RELATIVE` (type 8) is supported. `JUMP_SLOT` (6) and
`GLOB_DAT` (7) panic at load time -- EDOS binaries are static-PIE and must
not produce these. `IRELATIVE` is unhandled (would require calling resolvers
at load time; not needed until a dynamic linker lands). A single writable
PT_LOAD containing all reloc targets is asserted; binaries with relocs spread
across multiple writable PT_LOADs will panic.

## Result

Cold-spawn of `/bin/sh` from the terminal: 238 ms -> 162 ms (-32%).
Boot-time parallel loads are unchanged (already I/O-bound via AHCI NCQ).
