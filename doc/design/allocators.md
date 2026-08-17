# The two allocators

There are two and they are unrelated.

- **Userspace**: `PoolAllocator` in `~/dev/edos_rt/src/allocator.rs`, the global
  allocator every program links through the std fork. Segregated free lists over
  boundary tags.
- **Kernel**: `kernel/src/allocator.rs`, per-CPU caches across eight size classes
  (32..4096 bytes) over a buddy-system heap.

Both are measured: `programs/allocbench` in the guest for the first,
`/proc/alloc_bench` for the second. They ask the same three questions in the same
shape so the two tables can be read side by side.

## What sent us here

Time-to-desktop was about 3 s locally and 6.3 s on an 8-vCPU WHPX guest, and the
userspace half of that was the larger. Decomposed on a four-core KVM guest, with
`edos-taskbar` as the subject:

| from | to | cost |
|---|---|---|
| init spawns it | `main` entered | 26 ms |
| `main` entered | window created and shown | 0.6 ms |
| window shown | panel published | **2.66 s** |

So process creation and the window system were not the cost. `strace` accounted
for almost none of the 2.66 s: 1267 syscalls totalling **59 ms**, and the publish
happened inside that same slice. `/proc/meminfo`'s `PageFaults` moved by **596**
across a whole restart, so it was not demand paging either. It was userspace CPU,
and the only thing that path does in quantity is allocate: parsing four TTF
faces, building a layout, measuring text.

Two hypotheses were tested and refuted before the third:

- **TLB shootdowns from allocator churn.** The startup made 840 `mmap` and 379
  `munmap` calls, and every `munmap` IPIs the other CPUs and waits. Refuted by
  sweeping CPU count: boot-to-panel was 2.78 s at 2 CPUs and 2.81 s at 8, flat
  where a per-CPU acknowledgement cost would grow.
- **Font parsing.** `fontdue::Font::from_bytes` parses a whole face up front and
  `edos_render::font` loads all four eagerly. Benchmarked on the host against the
  same TTFs: 16.0, 11.8, 10.9 and 3.2 ms, **42 ms for all four**. Real, but two
  orders of magnitude short.

## What was actually wrong

Three separate linear walks, of which the free-list search named in the first
write-up was not the largest.

1. `allocate_from_free_list` walked one address-ordered list first-fit.
2. `add_to_free_list` walked the **same list again** to find the address the
   freed block sorted after. The list was kept in address order precisely so that
   both neighbours were known at the insertion point and could be merged, so
   coalescing was what made every free linear.
3. `allocate_from_chunks` walked every chunk whenever the newest one could not
   satisfy a request, which is every time a chunk fills.

On top of that `release_chunk` walked the chunk list on every free of a block
over a page and unmapped any chunk that had gone idle, with no hysteresis, so a
live set oscillating across a chunk boundary mapped and unmapped the same 64 KiB
over and over. That is where the 840 `mmap` / 379 `munmap` came from.

Measured by running the real algorithm on the host under a mixed workload of
200,000 allocations: **923 nodes walked per allocation, 1746 per free**, 2168
mappings against 1262 unmappings, 14.9 us per allocation.

### The plateau was not real

`allocbench`'s guest numbers plateaued between 500 and 4000 live blocks, which a
first-fit walk cannot do, and the first write-up flagged the mechanism as
unproven because of it.

It was the benchmark. The first `scaling` round is the only one whose
allocations come from memory the process has never touched, so it paid a fault
per page on top of every allocation; every later round inherited a warm heap from
the one before. The old allocator was slow enough everywhere that the extra
1.5 us did not stand out, and the flat top it produced read as a property of the
allocator. `scaling` now builds and drops the largest population before it starts
timing, and the sweep is monotonic.

The lesson is the general one: **a benchmark's first iteration measures the
memory system, not the thing under test.**

## What the userspace allocator is now

Every block carries its size in the word before the user pointer, and a free
block repeats that size in its last word. A block therefore reaches both of its
physical neighbours in constant time: forward by adding its own size, backward by
reading the trailing size of the block before it, which is only there to read when
a flag in its own header says that neighbour is free. So a free merges with
whatever is adjacent without searching for it, and coalescing stops costing a
walk.

Free blocks are filed in bins by size, one bin per exact size up to 512 bytes and
one per power of two above that, with a bitmap over the bins so the smallest
non-empty bin that certainly fits is found without looking at the empty ones.
Allocation pops a list head.

Two things sit on top of that shape:

- **Fast bins**, glibc's. A block freed at the top of a chunk touches the chunk's
  own remainder, so merging it and splitting it apart again is the entire cost of
  an allocate-and-free pair at one size, which is the cheapest thing a program
  does and the one it does most. Blocks up to 256 bytes are parked without
  merging, still marked in use so nothing merges through them. They are given back
  before the kernel is ever asked for more memory, so parking one cannot cause a
  mapping that would not have happened anyway.
- **In-place `realloc`.** A growing buffer is usually the most recent thing
  allocated, so the space after it is the free remainder of the chunk being
  carved. Reaching it also needed `library/std/src/sys/alloc/edos.rs` to stop
  calling `realloc_fallback`, which copied on every step.

An idle chunk is handed back only once the bins hold a chunk's worth of free
bytes besides it, which is the hysteresis the old one lacked.

`PoolAllocator::check_integrity` walks every chunk verifying the tags, the bin
membership and the free-byte total. It is the thing to reach for when a program
corrupts its own heap, ahead of guessing.

## Numbers

Guest, four-core KVM, `allocbench`. "before" is the address-ordered list; both
columns are medians of four runs.

| | before | after |
|---|---|---|
| alloc+free, nothing live | 15 ns | 22 ns |
| 500 live blocks | 1686 | 46 |
| 1000 live blocks | 1599 | 65 |
| 2000 live blocks | 1787 | 76 |
| 4000 live blocks | 1764 | 74 |
| 8000 live blocks | 4978 | 100 |
| after freeing 8000 | 86 | 92 |
| growth chain, 8 B to 32 KiB | not measured | 152 ns/step |

Boot-to-panel, from `Spawned bin/edos-init` to the taskbar's first `panel|` line
in `run_log.txt`, went from about 2.7 s to **150 ms**.

The point is not the ratio, it is that the column no longer has a slope.

**The floor regressed, 15 ns to 22.** `churn` cycles 24, 64, 256 and 1024 bytes,
and the last two span more than `FAST_MAX`, so half its allocations pay a merge
and a split that the old allocator did not. `FAST_MAX` is the lever if it ever
matters; it was left at 256 because covering 1 KiB means 63 exact-size bins and a
program holding one of each, to buy back 7 ns.

**One spike per run, of about 1.1 us per allocation, lands on a different
population every time.** It is a single ~2.3 ms event inside one round, not a
property of the population; read the median across runs rather than one run.

Host, running the same algorithm outside the guest, where node counts rather than
times are the point:

| | before | after |
|---|---|---|
| free-list nodes walked per allocation | 923 | 0 |
| nodes walked per free | 1746 | 0 |
| mappings / unmappings over 200k allocations | 2168 / 1262 | 940 / 142 |
| 200k mixed allocations | 14856 ns | 172 ns |

## The kernel allocator

It did **not** have the userspace problem, and the benchmark is what establishes
that rather than a reading of the code: cost is flat in the live population, 18,
18 and 16 ns/alloc at 500, 2000 and 8000 blocks, and 18 ns to reuse memory just
freed. Nothing there needs rewriting.

What it did have was a cliff at the top of its size classes. Everything a per-CPU
cache does not cover takes the one global heap lock, and the classes stopped at
1024 bytes, so a page-sized allocation, which the kernel makes constantly,
serialised against every other CPU:

| request | before | after |
|---|---|---|
| 16..1024 bytes | 15-17 ns | 17-18 ns |
| 2048 bytes | 39 ns | 19 ns |
| 4096 bytes | 63 ns | 20 ns |
| 16384 bytes | 20 ns | 22 ns |

Adding the 2048 and 4096 classes is the whole change. What a class may park per
CPU is now derived from one byte budget rather than a slot count, so a class large
enough for the budget to bind holds fewer objects: 16 of the 1024-byte class, 8 of
the 2048, 4 of the 4096. The heap held 128 KiB more afterwards on a four-CPU boot,
which is that budget exactly.

The single-CPU table cannot see the contention this removes, which is the larger
half of the argument for it. Measuring that wants several CPUs allocating at once
and a different instrument.

## Traps

- **The userspace allocator lives in another repo and ships through crates.io.**
  Changing it is the full loop from `CLAUDE.md`: patch `~/dev/edos_rt`, bump the
  version, `cargo publish`, move the `edos_rt` pin in the fork's
  `library/std/Cargo.toml`, `cargo +nightly update`, `./x install`, then
  `rm -rf programs/target && SCCACHE_RECACHE=1 make programs`. Test against a
  `[patch.crates-io]` path override first; a published version cannot be
  withdrawn.
- **`make all` does not regenerate `sata-disk.img`,** and root selection prefers a
  real disk, so a rebuilt userspace is invisible until
  `make clean-sata && make sata-disk.img`. Two measurements were lost to this.
- **Measure the algorithm on the host, the system in the guest.** How many nodes a
  walk visits is a property of the code and is the same either way, which is what
  made the mechanism findable in minutes instead of in publish cycles. What an
  allocation *costs* is the guest's own, and the host says nothing about it.
- **`/proc/alloc_bench`'s first read is not like the ones after it.** The kernel
  heap only grows, so the first run pays for the expansions its population forces.
  Read it twice.

## What is still open

- **One lock for the whole userspace heap.** Eight threads on the same heap cost
  699 ns per operation against 172 for one, which is the spin lock and nothing
  else. The answer is a thread-local front-end in the shape of mimalloc's or
  tcmalloc's; the obstacle is that it wants `#[thread_local]` inside the allocator
  that std's own TLS setup allocates through, which is a bootstrap problem worth
  its own change. Worth doing once something in the tree is thread-bound. Nothing
  is today.
- **Kernel allocator contention is unmeasured.** See above.
