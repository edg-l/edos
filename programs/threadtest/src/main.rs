//! Exercises thread spawn and join.
//!
//! `join` goes through `edos_rt::process::thread_join`, which blocks in the
//! kernel; the sleeping worker below is the case where a polling join would
//! spin through its whole sleep.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    // `threadtest nojoin` spawns and never joins, which separates a fault in
    // the clone/exit path from one in the blocking join.
    if std::env::args().nth(1).as_deref() == Some("nojoin") {
        for i in 0..8u64 {
            thread::spawn(move || i * i);
        }
        println!("spawned 8, not joining; sleeping");
        thread::sleep(Duration::from_secs(2));
        println!("nojoin: survived");
        return;
    }

    // `threadtest hammer` drives the allocator from more threads than the
    // machine has CPUs, and checks each block still holds what the owning
    // thread wrote, so a block handed out twice shows up as wrong data.
    if std::env::args().nth(1).as_deref() == Some("hammer") {
        let bad = Arc::new(AtomicU64::new(0));
        let workers: Vec<_> = (0..8u8)
            .map(|t| {
                let bad = Arc::clone(&bad);
                thread::spawn(move || {
                    let tag = t | 0x40;
                    let mut held: Vec<Vec<u8>> = Vec::new();
                    for i in 0..20_000usize {
                        held.push(vec![tag; 16 + (i * 37) % 400]);
                        if held.len() > 64 {
                            let v = held.swap_remove(i % 64);
                            if v.iter().any(|&b| b != tag) {
                                bad.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    held.iter().filter(|v| v.iter().any(|&b| b != tag)).count()
                })
            })
            .collect();
        let mut mismatched = 0;
        for w in workers {
            mismatched += w.join().unwrap_or_else(|_| {
                bad.fetch_add(1, Ordering::Relaxed);
                0
            });
        }
        let total = bad.load(Ordering::Relaxed) + mismatched as u64;
        println!("hammer: {total} corrupt blocks");
        return;
    }

    let mut failures = 0;

    // Values come back through the join handle.
    let handles: Vec<_> = (0..8u64)
        .map(|i| thread::spawn(move || i * i))
        .collect();
    let sum: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let expected: u64 = (0..8u64).map(|i| i * i).sum();
    if sum == expected {
        println!("join returns values: ok ({sum})");
    } else {
        println!("join returns values: FAIL (got {sum}, want {expected})");
        failures += 1;
    }

    // Shared state stays coherent across threads.
    let counter = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                for _ in 0..1000 {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let total = counter.load(Ordering::Relaxed);
    if total == 4000 {
        println!("shared counter: ok ({total})");
    } else {
        println!("shared counter: FAIL (got {total}, want 4000)");
        failures += 1;
    }

    // Joining a thread that sleeps must wait for it, not return early.
    let start = Instant::now();
    let handle = thread::spawn(|| {
        thread::sleep(Duration::from_millis(500));
        "done"
    });
    let word = handle.join().unwrap();
    let waited = start.elapsed();
    if word == "done" && waited >= Duration::from_millis(450) {
        println!("join waits for a sleeper: ok ({waited:?})");
    } else {
        println!("join waits for a sleeper: FAIL ({word}, waited {waited:?})");
        failures += 1;
    }

    // A heap-heavy worker, to run the allocator from more than one thread.
    let handles: Vec<_> = (0..4)
        .map(|t| {
            thread::spawn(move || {
                let mut held: Vec<Vec<u8>> = Vec::new();
                for i in 0..2000 {
                    held.push(vec![t as u8; 16 + (i % 400)]);
                    if held.len() > 100 {
                        held.swap_remove(i % 100);
                    }
                }
                held.len()
            })
        })
        .collect();
    let lens: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    // Each iteration pushes and then trims back to the cap, so the working set
    // ends at the cap.
    if lens.iter().all(|&l| l == 100) {
        println!("concurrent allocation: ok");
    } else {
        println!("concurrent allocation: FAIL ({lens:?})");
        failures += 1;
    }

    if failures == 0 {
        println!("threadtest: all passed");
    } else {
        println!("threadtest: {failures} failed");
        std::process::exit(1);
    }
}
