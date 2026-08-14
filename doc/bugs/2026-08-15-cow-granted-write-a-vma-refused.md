# The COW path granted write permission the VMA refused

## Status

Fixed in `kernel/src/memory/cow.rs` alongside the `mprotect` syscall. Covered by
`mmaptest` tests 12 and 13, both watched go red against the unguarded handler.
No follow-up outstanding.

## Symptoms

A store to a read-only private mapping succeeded in a forked child instead of
killing it. There was no crash, no log line, and no corruption anywhere the
process could see: the write simply landed on memory that was mapped
`PROT_READ`, and a subsequent read returned the stored byte.

Two ways to reach it, one of which needed no new syscall at all:

- `mmap(PROT_READ | MAP_PRIVATE | MAP_ANONYMOUS)`, read one byte to fault the
  page in, `fork`, then store to it from the child.
- Any writable private mapping that `mprotect` later narrowed to `PROT_READ`.

The first has been reachable since fork was written. Nothing in the tree does
it, which is why it went unnoticed.

## Root cause

`handle_cow_fault` decided entirely from the page table entry. It checked
`COW_BIT`, copied the frame when the refcount said the page was shared, and set
`WRITABLE` — never consulting the VMA that owns the address.

The fork walk in `clone_user_page_tables_cow` sets `COW_BIT` on **every**
anonymous present PTE, without asking whether that PTE was writable. A
read-only page therefore comes out of fork carrying the same mark as a writable
one, and the two are indistinguishable to the fault handler.

The dispatch order in `interrupts/idt.rs` completes the hole. A write to a
present read-only page raises `PROTECTION_VIOLATION | CAUSED_BY_WRITE |
USER_MODE`, which is exactly the COW candidate test, so the COW handler runs
*before* anything reads `vma.prot`. When it returns `true` the fault is over.
`handle_demand_fault`, which does check `vma.prot` and would have rejected the
access as `WriteToReadOnly`, is never reached — and could not help anyway, since
it declines every protection violation on its first line.

So the page table was being trusted as the authority on what a mapping may do,
when the VMA is the authority and the page table is a cache of it that fork had
deliberately made lossy.

The fix is one check at the top of `handle_cow_fault`: look up the VMA covering
the faulting address and decline the fault unless it contains `VmaProt::WRITE`.
Declining routes the fault to the kill path, which is the correct outcome.

## Reasoning rules going forward

- **The VMA is the authority on protection; a PTE is a cache of it.** Any path
  that *grants* a permission must derive it from `vma.prot`, not from what it
  finds in the entry. Paths that merely honour an existing entry are fine.
- **`COW_BIT` means "this frame may be shared", not "this page is writable".**
  The fork walk marks read-only anonymous pages too. Do not read the bit as a
  statement about permission.
- **A handler that returns "handled" before the protection check owns that
  check.** The COW handler runs ahead of `handle_demand_fault` by design, and
  `handle_demand_fault` refuses protection violations outright, so there is no
  second line of defence behind the COW path.
- `mprotect` follows the same rule from the other side: it writes `vma.prot`
  first, and edits present PTEs rather than rebuilding them, so `COW_BIT` and
  the PAT bits of a write-combining mapping survive. It deliberately leaves a
  COW page unwritable even when granting write — the first store faults, copies,
  and takes its permission from the VMA.

## If this reappears

The signature is a store that should have been fatal and was not. There will be
no log line, because nothing failed.

1. Confirm the VMA disagrees with the hardware: `/proc/<tid>/maps` shows the
   range's `rwx` from `vma.prot`. If it reads `r--` and the process is writing
   there happily, this is the bug class.
2. Read the PTE. The `PF-walk:` dump on a KILL prints the flags at each level;
   a page in this state has `COW_BIT` set and `WRITABLE` clear before the fault
   and `WRITABLE` set after it.
3. Distinguish from the neighbouring class — a *correct* COW fault on a
   genuinely writable mapping — by the VMA, never by the PTE. Both look
   identical in the page table.
4. `mmaptest /var` tests 12 and 13 cover both routes. Test 13 needs no
   `mprotect`, so a red there means the fork walk or the COW handler, not the
   syscall.

## Saved artifacts

None kept; the red is a two-minute reproduction with `mmaptest`.
