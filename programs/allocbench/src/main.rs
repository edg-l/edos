//! What an allocation costs, and whether that cost depends on how many
//! allocations came before it.
//!
//! A program's startup is mostly allocation: parsing a font, building a
//! layout, decoding an image. None of that shows in `strace`, because none of
//! it is a syscall, and none of it shows as page faults once the heap has the
//! pages. So an allocator that walks a list to find a block is invisible from
//! outside and pays for itself in seconds.
//!
//! The number that matters is not ns/alloc at one size. It is whether ns/alloc
//! *grows with the number of live blocks*: a constant means the allocator finds
//! a block without looking at the others, and growth means every allocation
//! walks what the program allocated before it.

use std::hint::black_box;
use std::time::Instant;

/// Sizes a real program asks for: a few words for a node, a string, a buffer.
const SIZES: [usize; 4] = [24, 64, 256, 1024];

fn main() {
    println!("allocbench: allocation cost against live-block count");

    churn();
    scaling();
    reuse();
    growth();
}

/// Allocate and free the same size in a loop. Nothing accumulates, so this is
/// the allocator's floor.
fn churn() {
    const ROUNDS: usize = 20_000;
    let start = Instant::now();
    for i in 0..ROUNDS {
        let v: Vec<u8> = Vec::with_capacity(SIZES[i % SIZES.len()]);
        black_box(&v);
    }
    let each = start.elapsed().as_nanos() as u64 / ROUNDS as u64;
    println!("churn      alloc+free, nothing live: {each} ns/op");
}

/// Hold `live` blocks, then time allocations made against that population.
///
/// Doubling `live` and watching ns/op double with it is what a per-allocation
/// walk of the free list looks like from here.
fn scaling() {
    // Build and drop the largest population first. Without it the first round
    // measured something else entirely: it is the only one whose allocations
    // come from memory the process has never touched, so it paid a page fault
    // per page on top of every allocation and read as an order of magnitude
    // worse than the rounds above it. The rest of the sweep inherits a warm
    // heap from the round before, so only the first was ever wrong.
    {
        let warm: Vec<Vec<u8>> = (0..8_000).map(|i| vec![0u8; SIZES[i % SIZES.len()]]).collect();
        black_box(&warm);
    }

    for live in [500usize, 1_000, 2_000, 4_000, 8_000] {
        // Build the population, then free every other block so the allocator
        // has a long list of holes rather than one clean tail to bump.
        let mut held: Vec<Vec<u8>> = (0..live)
            .map(|i| vec![0u8; SIZES[i % SIZES.len()]])
            .collect();
        for i in (0..held.len()).step_by(2) {
            held[i] = Vec::new();
        }

        const SAMPLES: usize = 2_000;
        let start = Instant::now();
        let mut sink: Vec<Vec<u8>> = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES {
            sink.push(vec![0u8; SIZES[i % SIZES.len()]]);
        }
        let each = start.elapsed().as_nanos() as u64 / SAMPLES as u64;
        black_box(&sink);
        black_box(&held);
        println!("scaling    {live:>5} live blocks: {each} ns/alloc");
    }
}

/// Free a large population and allocate again at the same sizes. An allocator
/// that reuses what it just freed answers from the free list; one that cannot
/// find the right hole asks the kernel for more.
fn reuse() {
    const N: usize = 8_000;
    let held: Vec<Vec<u8>> = (0..N).map(|i| vec![0u8; SIZES[i % SIZES.len()]]).collect();
    drop(held);

    let start = Instant::now();
    let again: Vec<Vec<u8>> = (0..N).map(|i| vec![0u8; SIZES[i % SIZES.len()]]).collect();
    let each = start.elapsed().as_nanos() as u64 / N as u64;
    black_box(&again);
    println!("reuse      after freeing {N}: {each} ns/alloc");
}

/// Grow one buffer by doubling, which is what every parser, every string built
/// a piece at a time and every `collect` into a `Vec` does.
///
/// The question is whether the allocator can extend a block in place. A buffer
/// being doubled is usually the most recent thing allocated, so the space after
/// it is the free remainder of whatever the allocator is carving from; an
/// allocator that cannot see that copies every byte it has, at every step, and
/// the copies dominate the growth.
fn growth() {
    const RUNS: usize = 2_000;
    const CAP: usize = 32 * 1024;

    let start = Instant::now();
    let mut steps = 0u64;
    for _ in 0..RUNS {
        let mut v: Vec<u8> = Vec::with_capacity(8);
        while v.capacity() < CAP {
            let target = v.capacity() * 2;
            v.resize(target, 0);
            steps += 1;
        }
        black_box(&v);
    }
    let each = start.elapsed().as_nanos() as u64 / steps.max(1);
    println!("growth     8 B -> {} KiB doubling: {each} ns/step", CAP / 1024);
}
