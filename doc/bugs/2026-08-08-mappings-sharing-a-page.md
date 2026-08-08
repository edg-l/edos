# Two mappings shared a page, so one silently zeroed the other

## Status

Fixed. `VmaSet::reserve` and `first_fit` now work in whole pages, and
`sys_mmap` / `sys_munmap` reject an unaligned address and round the length
up. `programs/vectest` is the regression test.

## Symptoms

A `Vec` grown past 64 KiB came back full of zeros. The length was right and
every byte was `0x00`:

```
lost data: len 65536 -> 69632, cap 65536 -> 131072, first bad offset 0 = 0x00
```

Downstream this looked like a networking bug. `wget` of a 300 KB file
reported `saved to '/tmp/big.bin' (0 bytes)`, because `read_to_end`
collected all 300204 bytes correctly and the *search* for the `\r\n\r\n`
header terminator then found nothing — the buffer it searched had been
zeroed under it, so the whole response was treated as headers with an empty
body.

The misleading part: `tcptest`, which reads into a fixed stack buffer and
never grows a `Vec`, pulled the identical response intact. Everything
pointed at the HTTP code and none of it was.

## Root cause

The MMU maps whole pages, so a mapping owns every page it touches.
`VmaSet::reserve` searched for a gap of `length` rounded **up** to a page,
then recorded the VMA with the **raw** `length`:

```rust
let start = self.find_free_address(hint, length)?;   // gap of round_up(length)
let end = start.as_u64().checked_add(length)?;       // VMA ends mid-page
```

`first_fit` starts its next search at a VMA's `end`, so once one mapping
ended mid-page the next one began *inside that same page*. Two mappings then
covered one page, and either could destroy the other:

- a zero-fill fault on the second one installs a fresh frame, discarding
  what the first had written there;
- `munmap` of the first unmaps a page the second is still using, after which
  the next touch faults in a fresh zero page.

The userspace allocator hits this on the first chunk whose length is not
exactly `CHUNK_SIZE`. `edos_rt`'s `PoolAllocator` sizes a chunk as
`max(64 KiB, request + metadata)`, so every chunk is exactly 65536 bytes and
page-aligned until an allocation larger than that arrives. Growing a `Vec`
to 128 KiB asks for a ~131136-byte chunk, whose mapping starts 65600 bytes
into the previous one's last page. `realloc` copies the old contents into
the new chunk, frees the old one, and `release_chunk` then unmaps the whole
64 KiB chunk — including the shared page holding the first 3.9 KiB of the
data just copied. The next read of the `Vec` faults that page back in as
zeros.

This was newly reachable rather than newly written. While
`find_free_address` was a monotonic bump allocator, every result was
page-aligned by construction. First fit made the cursor follow VMA ends, and
VMA ends were unrounded.

## Reasoning rules going forward

- **A kernel-placed mapping covers whole pages.** `reserve` rounds the
  length, `first_fit` rounds every cursor it derives from a VMA end, and a
  `debug_assert` in `reserve` checks its own output. Not every VMA is
  page-aligned — an ELF segment's bound comes from `p_memsz` — which is
  exactly why the *search* has to round rather than trusting the tree.
- **Page granularity is enforced at the syscall boundary too.** `sys_mmap`
  and `sys_munmap` reject an address that is not page-aligned instead of
  rounding it silently, since rounding would handpocket memory the caller
  never asked for, or unmap a neighbour.
- **A correct length does not mean correct contents.** The failure here
  reported the right byte count at every layer.

## If this reappears

Run `vectest`. It grows one `Vec` by `extend_from_slice` to 2 MiB and
verifies every byte after each step, printing the exact capacity transition
that lost data:

```
lost data: len <before> -> <after>, cap <before> -> <after>, first bad offset N
```

A first bad offset of 0 with a correct length means the buffer's backing
pages were replaced, not that the writer wrote wrong data. Distinguish from
the neighbouring bug classes:

- **Contents wrong but not zero** — an aliasing bug: two live allocations
  over the same range (see the concurrent-`mmap` case in `WORKING-NOTES.md`).
- **Contents zero from an offset inside the buffer** — one page lost, so
  suspect a shared page at that boundary.
- **Contents zero from offset 0** — the first page of the allocation was
  unmapped and refaulted, which is this bug.

`log_debug!` on the mmap path (`loglevel=debug`) prints every mapping's
address and length; two consecutive lines where the second address falls
inside the first's last page name the fault directly.
