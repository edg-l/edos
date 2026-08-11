# A single-CPU boot never flushed its own TLB after an unmap

## Status

Fixed. `shootdown_needed()` is gone and every unmap path calls
`tlb_shootdown` unconditionally.

## Symptoms

Boot with one CPU (`make run-single`, or `scripts/edos-vm start --smp 1`) and
the desktop never comes up. `edos-taskbar` and `edos-terminal` each take a
general protection fault within ~50 ms of starting, `edos-init` restarts them
five times and gives up:

```
[2.383378] <cpu-0:/bin/edos-taskbar:u:24> GPF Error code: 0x0
[2.383381] <cpu-0:/bin/edos-taskbar:u:24> Selector index: 0
    instruction_pointer: VirtAddr(0x465fd7),
    code_segment: SegmentSelector { index: 4, rpl: Ring3 },
init: edos-taskbar exited with 135 after only 49.398ms (failure 1)
```

Both faults resolve to the same instruction, and it is not in either program:

```
$ addr2line -e programs/target/x86_64-unknown-edos/debug/edos-taskbar -f 0x65fd7
<edos_rt::allocator::PoolAllocator as core::alloc::global::GlobalAlloc>::dealloc
```

Subtract the `0x400000` load base from the faulting RIP before resolving it —
the programs are PIE.

The instruction is `mov %rcx,(%r8)`, the first of the two stores that
`FreeBlock::new` makes when `add_to_free_list` links a freed block. A #GP with
error code 0 on a store means the address was non-canonical, so the allocator
had read a corrupt chunk header. The same boot on four CPUs was perfectly
healthy, which is the wrong way round for a race and was the clue.

## Root cause

`munmap` tears down a range by unmapping each page with the returned
`MapperFlush` **ignored**, and then flushing the whole range once:

```rust
if let Ok((_, flush)) = mm.mapper.unmap(page) {
    flush.ignore();
    frames.push(phys);
}
...
if !frames.is_empty() && crate::memory::tlb::shootdown_needed() {
    crate::memory::tlb::tlb_shootdown(vma_start, vma_page_count);
}
```

`shootdown_needed()` was `cpu_count() > 1`. On one CPU it returned false and
the range was never invalidated anywhere — the per-page flush had already been
dropped on the floor. The frames went straight back to the frame allocator with
the faulting CPU still holding live translations to them. The next mapping to
land on those virtual addresses inherited the stale entries, so the process read
and wrote whatever the frames had since been reused for. `edos_rt`'s
`PoolAllocator` releases idle chunks with `munmap` and maps new ones as it
grows, which is why a GUI program starting up was the first thing to die.

The guard's intent was to skip the IPI round when nobody else is running.
`tlb_shootdown` already does exactly that, in its own fast path, and does the
local flush on the way. The guard therefore never saved anything the callee
would not have saved anyway, and cost the one thing the callee was there to do.

Three of the five call sites dropped their per-page flush and so were broken
(`sys_munmap` for anonymous and file-backed VMAs, and the page-cache
invalidation in `vfs.rs`). The other two flushed each page as they went and were
merely redundant.

## Reasoning rules going forward

- **A batched unmap owes the local TLB an invalidation, and one CPU does not
  make that free.** `flush.ignore()` is a promise to flush the range later, not
  a decision that no flush is needed. Whoever writes the `ignore()` owns the
  matching `tlb_shootdown`, unconditionally.
- **Do not guard a call on a condition the callee already handles.** The guard
  and the callee's fast path tested the same thing, and only the callee knew
  what still had to happen when it was false.
- **An SMP-shaped bug that only appears with *fewer* CPUs is not a race.** It
  is a path that only runs when the CPU count is small — which almost always
  means a count-dependent branch someone wrote as an optimisation.

## If this reappears

1. Resolve the faulting RIP minus `0x400000` against the program with
   `addr2line`. Landing inside the allocator means corrupt heap metadata, not
   an allocator bug.
2. Compare a one-CPU boot against a four-CPU boot. If only the small one
   breaks, `grep` for `cpu_count()` and `online_cpu_mask()` in the path.
3. `/proc/meminfo` and `/proc/processes` will look normal — the frames are
   accounted for correctly, they are just visible to someone who should have
   lost sight of them.
