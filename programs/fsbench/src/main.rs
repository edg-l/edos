//! Filesystem throughput and latency benchmark.
//!
//! Measures the same bytes through the idioms a program can choose between,
//! at three depths: the filesystem, the raw block device, and (by pointing it
//! at `/tmp`) a memory filesystem that never reaches a disk at all. Reading
//! the three together is the point. `/tmp` is the ceiling the syscall and copy
//! path imposes; `fsbench raw /dev/sda` is the ceiling the block layer and the
//! AHCI driver impose; the on-disk numbers are what is left after EFS.
//!
//! Cold reads need two boots. `fsbench write DIR` leaves its files behind and
//! `fsbench read DIR` reads them, so a reboot in between makes the read phase
//! genuinely cold. `fsbench all DIR` does both in one boot and its read
//! numbers are page-cache hits, which is useful for measuring the cache and
//! useless for measuring the disk.

mod counters;
mod harness;
mod workloads;

use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use edos_lib::sys::{SYS_SYNC, syscall0};

use counters::Counters;
use harness::{Budget, Report, human_bytes};
use workloads::SWEEP;

/// Per-benchmark time budget. Every test stops after roughly this long, so the
/// suite's runtime does not depend on how fast the filesystem is.
const DEFAULT_BUDGET_MS: u64 = 700;

/// Per-benchmark byte cap, so a fast path stops before it fills the disk.
const DEFAULT_CAP_MIB: u64 = 256;

/// Working-set size for the random and mapped tests.
const WORKING_SET: u64 = 16 << 20;

/// Span covered by the raw device sweep. Larger than the 8 MiB block page
/// cache by enough that a pass evicts what the previous pass left, so the
/// reads keep reaching the driver instead of the cache.
const RAW_SPAN: u64 = 256 << 20;

/// Where the raw sweep starts. Past the partition table and any filesystem
/// metadata that a mounted root keeps hot.
const RAW_SKIP: u64 = 64 << 20;

struct Options {
    mode: Mode,
    path: String,
    budget_ms: u64,
    cap_mib: u64,
    max_ops: Option<u64>,
    verify: bool,
    keep: bool,
    klog: bool,
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    All,
    Write,
    Read,
    Raw,
    RawWrite,
    Clean,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::All => "all",
            Mode::Write => "write",
            Mode::Read => "read",
            Mode::Raw => "raw",
            Mode::RawWrite => "rawwrite",
            Mode::Clean => "clean",
        }
    }
}

/// Report sink: stdout, and optionally the kernel log as well.
///
/// The guest terminal holds far fewer lines than a full run prints, and
/// `/dev/klog` is teed to the host's serial capture, so `--klog` is how a whole
/// run gets read off a headless machine.
struct Out {
    klog: Option<std::fs::File>,
}

impl Out {
    fn new(enabled: bool) -> Self {
        Self {
            klog: enabled
                .then(|| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/klog")
                        .ok()
                })
                .flatten(),
        }
    }

    fn line(&mut self, text: &str) {
        println!("{text}");
        if let Some(klog) = &mut self.klog {
            let _ = writeln!(klog, "{text}");
        }
    }

    fn blank(&mut self) {
        self.line("");
    }

    fn header(&mut self, title: &str) {
        self.blank();
        self.line(&format!(
            "{title:<33} {:>8} {:>9} {:>8} {:>8} {:>9}",
            "MiB/s", "ops/s", "p50", "p99", "max"
        ));
    }

    /// Print one result. Returns 1 if the benchmark failed, for the exit code.
    fn report(&mut self, r: &Report) -> u32 {
        let label = format!("{} {}", r.name, r.unit);
        if let Some(error) = &r.error {
            self.line(&format!("{label:<33} SKIP: {error}"));
            return 1;
        }
        let throughput = if r.bytes == 0 {
            "-".to_string()
        } else {
            rate(r.mib_per_sec())
        };
        self.line(&format!(
            "{label:<33} {throughput:>8} {:>9} {:>8} {:>8} {:>9}",
            rate(r.ops_per_sec()),
            duration(r.latency.p50),
            duration(r.latency.p99),
            duration(r.latency.max),
        ));
        0
    }
}

fn usage() -> ! {
    eprintln!(
        "\
Usage: fsbench [MODE] [PATH] [OPTIONS]

Modes:
  all     write, read and metadata in one boot (default). Read numbers are
          page-cache hits, not disk reads.
  write   write and metadata only; leaves its files for a later read run
  read    read only, against files a previous `write` run left behind
  raw     sequential reads straight from a block device, no filesystem
  rawwrite  sequential writes straight to a block device. DESTROYS whatever is
          on it; refused while a filesystem on that device is mounted
  clean   remove every file the suite creates

Paths:
  a directory for all/write/read (default /var), a device for raw (/dev/sda)

Options:
  -t MS     per-test time budget in ms (default {DEFAULT_BUDGET_MS})
  -m MIB    per-test byte cap in MiB (default {DEFAULT_CAP_MIB})
  -n OPS    fixed operations per test, overriding -t and -m. Use this to
            compare two builds: a time budget makes the faster one do more
            work and meet every later test with a different filesystem.
  -q        quick: 200 ms budget
  -k        keep the files a run creates (implied by `write`)
  -l        mirror the report to /dev/klog as well as stdout
  --no-verify   skip the post-write pattern check

Examples:
  fsbench                    on-disk filesystem, one boot
  fsbench /tmp               memfs: the syscall and copy ceiling
  fsbench raw /dev/sda       the block layer and AHCI ceiling
  fsbench write /var         ... reboot ...   fsbench read /var"
    );
    std::process::exit(2)
}

fn parse_args() -> Options {
    let mut opts = Options {
        mode: Mode::All,
        path: String::new(),
        budget_ms: DEFAULT_BUDGET_MS,
        cap_mib: DEFAULT_CAP_MIB,
        max_ops: None,
        verify: true,
        keep: false,
        klog: false,
    };
    let mut positional: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => usage(),
            "-q" => opts.budget_ms = 200,
            "-k" => opts.keep = true,
            "-l" => opts.klog = true,
            "--no-verify" => opts.verify = false,
            "-t" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => opts.budget_ms = v,
                None => usage(),
            },
            "-m" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => opts.cap_mib = v,
                None => usage(),
            },
            "-n" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => opts.max_ops = Some(v),
                None => usage(),
            },
            other if other.starts_with('-') => usage(),
            other => positional.push(other.to_string()),
        }
    }

    let mut rest = positional.into_iter();
    if let Some(first) = rest.next() {
        match first.as_str() {
            "all" => opts.mode = Mode::All,
            "write" => opts.mode = Mode::Write,
            "read" => opts.mode = Mode::Read,
            "raw" => opts.mode = Mode::Raw,
            "rawwrite" => opts.mode = Mode::RawWrite,
            "clean" => opts.mode = Mode::Clean,
            _ => opts.path = first,
        }
    }
    if opts.path.is_empty()
        && let Some(second) = rest.next()
    {
        opts.path = second;
    }
    if rest.next().is_some() {
        usage();
    }
    if opts.path.is_empty() {
        opts.path = match opts.mode {
            Mode::Raw | Mode::RawWrite => "/dev/sda".to_string(),
            _ => "/var".to_string(),
        };
    }
    if opts.mode == Mode::Write {
        opts.keep = true;
    }
    opts
}

fn main() -> ExitCode {
    let opts = parse_args();
    let mut out = Out::new(opts.klog);

    if opts.mode == Mode::Clean {
        workloads::cleanup(&opts.path);
        out.line(&format!(
            "fsbench: removed benchmark files under {}",
            opts.path
        ));
        return ExitCode::SUCCESS;
    }

    let budget = Budget::new(opts.budget_ms, opts.cap_mib << 20, opts.max_ops);
    out.line(&format!(
        "fsbench {} on {}  ({})",
        opts.mode.name(),
        opts.path,
        match opts.max_ops {
            Some(n) => format!("{n} ops per test"),
            None => format!(
                "{} ms or {} per test",
                opts.budget_ms,
                human_bytes(opts.cap_mib << 20)
            ),
        }
    ));

    let before = Counters::sample();
    let started = Instant::now();
    let mut failures = 0u32;

    match opts.mode {
        Mode::Raw => {
            out.header("RAW DEVICE READ");
            for &chunk in SWEEP {
                let report = workloads::raw_read(&opts.path, chunk, RAW_SKIP, RAW_SPAN, budget);
                failures += out.report(&report);
            }
        }
        Mode::RawWrite => {
            out.header("RAW DEVICE WRITE");
            for &chunk in SWEEP {
                let report = workloads::raw_write(&opts.path, chunk, RAW_SKIP, RAW_SPAN, budget);
                failures += out.report(&report);
            }
        }
        Mode::Read => failures += read_phase(&mut out, &opts.path, budget),
        Mode::Write | Mode::All => {
            failures += write_phase(&mut out, &opts.path, budget);
            if opts.verify {
                let problems = workloads::verify(&opts.path);
                out.blank();
                if problems.is_empty() {
                    out.line("verify: all patterns match");
                } else {
                    failures += problems.len() as u32;
                    for p in &problems {
                        out.line(&format!("VERIFY FAIL: {p}"));
                    }
                }
            }
            if opts.mode == Mode::All {
                failures += read_phase(&mut out, &opts.path, budget);
            }
        }
        Mode::Clean => unreachable!(),
    }

    let deltas = before.delta(&Counters::sample());
    if !deltas.is_empty() {
        out.blank();
        out.line("KERNEL COUNTERS");
        for (key, value) in deltas {
            out.line(&format!("  {key:<34} {value}"));
        }
    }

    out.blank();
    out.line(&format!("total {:.1}s", started.elapsed().as_secs_f64()));

    if !opts.keep && opts.mode != Mode::Read {
        workloads::cleanup(&opts.path);
    }

    if failures > 0 {
        out.line(&format!("{failures} test(s) failed"));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn write_phase(out: &mut Out, dir: &str, budget: Budget) -> u32 {
    let mut failures = 0;

    out.header("WRITE");
    for &chunk in SWEEP {
        failures += out.report(&workloads::write_seq(dir, chunk, budget));
    }
    failures += out.report(&workloads::write_buffered(dir, budget));
    failures += out.report(&workloads::write_whole_file(dir, 1 << 20, budget));
    failures += out.report(&workloads::write_positional(dir, 65536, budget));
    for &chunk in SWEEP {
        failures += out.report(&workloads::overwrite_seq(dir, chunk, budget));
    }
    failures += out.report(&workloads::write_random(dir, WORKING_SET, budget));
    // Every row above stops timing while the bytes are still in the page
    // cache. These two are the durable numbers.
    for &chunk in &[4096usize, 1 << 20] {
        failures += out.report(&workloads::write_durable(dir, chunk, budget));
    }
    failures += out.report(&workloads::write_mmap(dir, 4 << 20, budget));

    // Durability is a separate cost from throughput, and it is the cost every
    // number above excludes: the writes are still in flight when their test
    // ends. Time the drain on its own line.
    let t0 = Instant::now();
    unsafe { syscall0(SYS_SYNC) };
    out.blank();
    out.line(&format!(
        "  sync() after the write phase: {}",
        duration(t0.elapsed().as_nanos() as u64)
    ));

    out.header("METADATA");
    failures += out.report(&workloads::meta_create_unlink(dir, budget));
    failures += out.report(&workloads::meta_stat(dir, budget));
    failures += out.report(&workloads::meta_readdir(dir, budget));

    failures
}

fn read_phase(out: &mut Out, dir: &str, budget: Budget) -> u32 {
    let mut failures = 0;

    out.header("READ");
    for &chunk in SWEEP {
        failures += out.report(&workloads::read_seq(dir, chunk, false, budget));
    }
    failures += out.report(&workloads::read_positional(dir, 65536, budget));
    failures += out.report(&workloads::read_whole_file(dir, 1 << 20, budget));
    failures += out.report(&workloads::read_random(dir, budget));
    failures += out.report(&workloads::read_mmap(dir, 4 << 20, budget));
    // Same file, same call size, guaranteed cache hit: the difference against
    // the first `read 1MiB` line is what the page cache is worth.
    failures += out.report(&workloads::read_seq(dir, 1 << 20, true, budget));

    failures
}

/// Format a rate with enough precision to stay readable when it is small.
///
/// A path doing one operation every 30 seconds is the most interesting row on
/// the page, and rounding it to `0` is how it gets missed.
fn rate(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 1.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.3}")
    }
}

/// Format nanoseconds in whichever unit keeps three significant digits.
fn duration(nanos: u64) -> String {
    if nanos < 1_000 {
        format!("{nanos}ns")
    } else if nanos < 1_000_000 {
        format!("{:.0}us", nanos as f64 / 1_000.0)
    } else if nanos < 1_000_000_000 {
        format!("{:.1}ms", nanos as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", nanos as f64 / 1_000_000_000.0)
    }
}
