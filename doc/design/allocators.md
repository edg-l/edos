# The two allocators

There are two, they are unrelated, and only one of them has been measured.

- **Userspace**: `PoolAllocator` in `~/dev/edos_rt/src/allocator.rs`, the global
  allocator every program links through the std fork. A first-fit walk of one
  linked free list.
- **Kernel**: `kernel/src/allocator.rs`, per-CPU caches across six size classes
  (32..1024 bytes, 16 objects each, refilled in batches of 8) over a
  buddy-system heap. Sound in shape; never benchmarked.

## What sent us here

Time-to-desktop is about 3 s locally and 6.3 s on an 8-vCPU WHPX guest, and the
userspace half of that is the larger. Decomposed on a four-core KVM guest, with
`edos-taskbar` as the subject:

| from | to | cost |
|---|---|---|
| init spawns it | `main` entered | 26 ms |
| `main` entered | window created and shown | 0.6 ms |
| window shown | panel published | **2.66 s** |

So process creation and the window system are not the cost. The 2.66 s is one
pass through the first loop iteration, and `strace` accounts for almost none of
it: 1267 syscalls totalling **59 ms**, and the publish happens inside that same
slice, before the loop ever waits. `/proc/meminfo`'s `PageFaults` moves by
**596** across a whole restart, so it is not demand paging either. It is
userspace CPU, and the only thing that path does in quantity is allocate:
parsing four TTF faces, building a layout, measuring text.

Two hypotheses were tested and refuted before the third:

- **TLB shootdowns from allocator churn.** The startup makes 840 `mmap` and 379
  `munmap` calls, and every `munmap` IPIs the other CPUs and waits. Refuted by
  sweeping CPU count: boot-to-panel is 2.78 s at 2 CPUs and 2.81 s at 8, flat
  where a per-CPU acknowledgement cost would grow. (Single-CPU is 5.47 s, but
  that boot has no parallelism between the three GUI programs, so it measures
  something else.)
- **Font parsing.** `fontdue::Font::from_bytes` parses a whole face up front and
  `edos_render::font` loads all four eagerly. Benchmarked on the host against
  the same TTFs: 16.0, 11.8, 10.9 and 3.2 ms, **42 ms for all four**. Real, but
  two orders of magnitude short.

## What `allocbench` measures

`programs/allocbench` times allocation against the number of blocks already
live, which is the question that separates an allocator that finds a block from
one that looks for it. On a four-core KVM guest:

| pattern | ns per allocation |
|---|---|
| alloc+free, nothing live | **15** |
| 500 live blocks | 1686 |
| 1000 live blocks | 1599 |
| 2000 live blocks | 1787 |
| 4000 live blocks | 1764 |
| 8000 live blocks | **4978** |
| after freeing all 8000 | 86 |

An allocation costs 15 ns when nothing is live and 1.7 µs when a few thousand
blocks are, which is over a hundred times more for the same call. At 1.7 µs,
2.66 s of startup is about 1.5 million allocations — the right order for
parsing four fonts and laying out a panel.

Note what the shape is *not*: it plateaus between 500 and 4000 rather than
rising with the population, then jumps at 8000. A pure first-fit walk would rise
throughout. So the walk is likely not the whole mechanism, and the next session
should find the rest rather than assume it. The measurement is the fact; the
mechanism is still a hypothesis.

## The userspace allocator today

`allocate_from_free_list` takes the list lock, walks from the head, and takes
the first block whose span fits, splitting the tail back into the list when the
remainder is at least `MIN_BLOCK`. Misses fall to `allocate_from_chunks`, which
walks the chunk list asking each to `reserve`, and then to `allocate_new_chunk`,
which asks the kernel for more. There are no size classes, so a 24-byte request
and a 1 KiB request search the same list, and nothing sorts it.

That last part shows from outside: 840 `mmap`s during one program's startup is
an allocator going back to the kernel rather than reusing what it has.

## Where to take it

In rough order of value, to be decided with numbers next session:

1. **Segregated free lists by size class** in the userspace allocator, so the
   common small allocation pops a head instead of searching. This is the change
   `allocbench`'s 15 ns floor says is available.
2. **Coalescing on free**, so a fragmented heap does not stay fragmented and the
   chunk list stops growing.
3. **Benchmark the kernel allocator** before touching it. Its shape is right,
   but nothing has measured the fall-through to the buddy heap, and the same
   question applies: what does an allocation cost when the heap is busy?

## Traps

- **The userspace allocator lives in another repo and ships through crates.io.**
  Changing it is the full loop from `CLAUDE.md`: patch `~/dev/edos_rt`, bump the
  version, `cargo publish`, move the `edos_rt` pin in the fork's
  `library/std/Cargo.toml`, `cargo +nightly update`, `./x install`, then
  `rm -rf programs/target && SCCACHE_RECACHE=1 make programs`. Test against a
  `[patch.crates-io]` path override first; a published version cannot be
  withdrawn.
- **`make all` does not regenerate `sata-disk.img`,** and root selection prefers
  a real disk, so a rebuilt userspace is invisible until
  `make clean-sata && make sata-disk.img`. Two measurements were lost to this.
- **Measure with the guest, not the host.** The host benchmark of `fontdue`
  above was useful precisely because it *refuted* a guest hypothesis; it says
  nothing about allocator behaviour, which is the guest's own.
