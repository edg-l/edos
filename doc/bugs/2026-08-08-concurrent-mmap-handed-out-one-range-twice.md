# Concurrent `mmap` handed the same range to several threads

## Status

Fixed in `26928b13`. `VmaSet::reserve` (`kernel/src/memory/vma.rs`) runs the
first fit and inserts the VMA under one acquisition of the `vmas` lock;
`find_free_address` is private; `syscalls::memory::claim_range` is the single
entry point.

## Symptoms

Memory corruption in any multi-threaded program. First seen as
`PoolAllocator` faulting on a free-list link read from an address like `0x28`
under `bin/threadtest hammer` (eight threads allocating hard). With
`loglevel=debug`, the log shows several threads of one process given the same
mapping:

```
thread-75: mmap: lazy mapped at 0x143b000
thread-76: mmap: lazy mapped at 0x143b000
thread-72: mmap: lazy mapped at 0x143b000
```

The symptom sent two separate investigations into the allocator. Both were
dead ends.

## Root cause

`sys_mmap` ran the first fit under one acquisition of the `vmas` lock and
inserted the `Vma` under a later one. Two threads could both find a range
free and both take it. Their allocator chunks then aliased the same pages, so
one thread's free-list links landed in the other's blocks.

First fit reuses freed ranges, so the aliasing lands in live memory rather
than an unmapped hole. That makes the damage far worse than under a bump
allocator.

Five call sites had the shape: anonymous, file-backed and `MAP_PHYSICAL`
`mmap`, `sys_shm_map`, and the 2 MiB thread stack in `sys_clone`. The last is
the worst: every `std::thread::spawn` goes through it, so two concurrent
spawns could share a stack. The paths that can fail after claiming
(`MAP_PHYSICAL` and `shm_map`) release the range on the way out.

Widening the guard across the page-table work was rejected: `vmas` is a
`PreemptSpinlock`, so every mapping would become one non-preemptible span.

## Reasoning rules going forward

- An address a search returns is free only while the lock that made the
  search true is held. Search and claim under one acquisition.
- Corruption inside an allocator's metadata is usually the allocator's pages
  being shared, not the allocator. Check the mapping layer first.
- A timing change is not a fix. Swapping the userspace allocator's lock
  primitive made the corruption appear and disappear across runs.

## If this reappears

1. Boot with `loglevel=debug` and collect `mmap: lazy mapped at` lines.
2. Count duplicate addresses per address space. Segment the log by process
   and keep only that process's own threads. Separate runs of a program are
   separate address spaces, and `mmaptest` execs two copies of `echo` that
   legitimately map at the same address; both make a naive check cry wolf.
3. A duplicate within one address space is this bug. Contents that are wrong
   but not zero point here; contents zeroed from an offset point at
   `2026-08-08-mappings-sharing-a-page.md`.
