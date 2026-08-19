//! Timing harness.
//!
//! Every benchmark is a loop of operations, each moving a known number of
//! bytes. [`Runner`] times each one and stops the loop once a time budget or a
//! byte cap is hit, whichever comes first. Sizing by time rather than by bytes
//! is what makes the suite finish: a path that runs at 1 MiB/s and one that
//! runs at 1 GiB/s both take about the same wall-clock second.
//!
//! Per-operation latency is kept, not just the total. A path that averages
//! 200 us but stalls for 2 s once is a bug, and only the maximum shows it.

use std::time::{Duration, Instant};

/// Number of latency samples kept per benchmark. Beyond this the reservoir
/// stops growing and only the maximum keeps updating, so a run of millions of
/// tiny operations cannot exhaust memory.
const MAX_SAMPLES: usize = 200_000;

#[derive(Clone, Copy)]
pub struct Budget {
    /// Stop starting new operations once this much time has passed.
    pub target: Duration,
    /// Stop starting new operations once this many bytes have moved.
    pub max_bytes: u64,
    /// Stop after exactly this many operations, ignoring the other two limits.
    ///
    /// A time budget makes two builds do *different amounts of work*, so the
    /// faster one arrives at every later test with a fuller, more fragmented
    /// filesystem and the comparison measures that instead of the change. Any
    /// A/B between builds wants this.
    pub max_ops: Option<u64>,
}

impl Budget {
    pub fn new(target_ms: u64, max_bytes: u64, max_ops: Option<u64>) -> Self {
        Self {
            target: Duration::from_millis(target_ms),
            max_bytes,
            max_ops,
        }
    }
}

/// Latency distribution over one benchmark's operations, in nanoseconds.
pub struct Latency {
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
}

pub struct Report {
    pub name: String,
    /// Op size the caller asked for, used only for display.
    pub unit: String,
    pub bytes: u64,
    pub ops: u64,
    pub elapsed: Duration,
    pub latency: Latency,
    /// Set when the workload could not run at all (missing file, ENOSPC).
    pub error: Option<String>,
}

impl Report {
    pub fn mib_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 || self.bytes == 0 {
            return 0.0;
        }
        (self.bytes as f64 / (1024.0 * 1024.0)) / secs
    }

    pub fn ops_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        self.ops as f64 / secs
    }
}

pub struct Runner {
    name: String,
    unit: String,
    budget: Budget,
    start: Instant,
    bytes: u64,
    ops: u64,
    samples: Vec<u64>,
    max_sample: u64,
    error: Option<String>,
}

impl Runner {
    pub fn new(name: &str, unit: &str, budget: Budget) -> Self {
        Self {
            name: name.to_string(),
            unit: unit.to_string(),
            budget,
            start: Instant::now(),
            bytes: 0,
            ops: 0,
            samples: Vec::new(),
            max_sample: 0,
            error: None,
        }
    }

    /// True while the budget still allows another operation. Always allows the
    /// first one, so a single operation larger than the whole budget is still
    /// measured rather than reported as zero.
    pub fn keep_going(&self) -> bool {
        if self.error.is_some() {
            return false;
        }
        if let Some(max_ops) = self.budget.max_ops {
            return self.ops < max_ops;
        }
        if self.ops == 0 {
            return true;
        }
        self.start.elapsed() < self.budget.target && self.bytes < self.budget.max_bytes
    }

    /// Time one operation. `bytes` is what the operation is expected to move;
    /// it is only counted when `f` reports success.
    pub fn op<T, E: std::fmt::Display>(
        &mut self,
        bytes: u64,
        f: impl FnOnce() -> Result<T, E>,
    ) -> Option<T> {
        let t0 = Instant::now();
        let result = f();
        let dt = t0.elapsed().as_nanos() as u64;

        match result {
            Ok(value) => {
                self.ops += 1;
                self.bytes += bytes;
                self.max_sample = self.max_sample.max(dt);
                if self.samples.len() < MAX_SAMPLES {
                    self.samples.push(dt);
                }
                Some(value)
            }
            Err(e) => {
                self.error = Some(e.to_string());
                None
            }
        }
    }

    /// Record a failure from setup code that is not itself timed.
    pub fn fail(&mut self, msg: String) {
        self.error = Some(msg);
    }

    pub fn finish(mut self) -> Report {
        let elapsed = self.start.elapsed();
        self.samples.sort_unstable();
        let pick = |q: f64| -> u64 {
            if self.samples.is_empty() {
                return 0;
            }
            let idx = ((self.samples.len() as f64 - 1.0) * q).round() as usize;
            self.samples[idx]
        };
        Report {
            name: self.name,
            unit: self.unit,
            bytes: self.bytes,
            ops: self.ops,
            elapsed,
            latency: Latency {
                p50: pick(0.50),
                p99: pick(0.99),
                max: self.max_sample,
            },
            error: self.error,
        }
    }
}

/// Deterministic 64-bit PRNG. Reproducible on purpose: two runs of the random
/// workloads must touch the same offsets or their numbers are not comparable.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `[0, n)`. `n` must be non-zero.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
        ("B", 1),
    ];
    for (suffix, scale) in UNITS {
        if n >= scale {
            if n.is_multiple_of(scale) {
                return format!("{}{}", n / scale, suffix);
            }
            return format!("{:.1}{}", n as f64 / scale as f64, suffix);
        }
    }
    format!("{n}B")
}
