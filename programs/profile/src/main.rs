//! `profile`: where the CPU actually is.
//!
//! Claims the kernel's sampling profiler, drains samples for a while, and
//! prints them folded — one line per distinct stack with the number of samples
//! that landed on it. Addresses stay raw: `scripts/profile-resolve` on the
//! build host turns them into symbols, because that is where the ELF files
//! with the DWARF in them live.
//!
//! Nothing here interprets a stack. A profile is a count of where the machine
//! was, and the shape of ten thousand of those is the answer; a single sample
//! means nothing at all.

use std::collections::HashMap;
use std::io::Write;

use edos_lib::process;
use edos_lib::profile::{self, SAMPLE_BROKEN_CHAIN, SAMPLE_IDLE, SAMPLE_TRUNCATED, Sample};

const USAGE: &str = "\
usage: profile [options] [PROGRAM [ARGS...]]

  -f HZ      samples per second per CPU (default 999)
  -d SEC     how long to sample for (default 5; ignored with a PROGRAM)
  -o FILE    write the profile to FILE instead of stdout
  -h         this message

With a PROGRAM, sampling runs until it exits. Without one, the whole machine
is sampled for the duration.

Sampling is machine-wide either way: the kernel samples every CPU, so the
profile shows everything running, not only the program named. Resolve the
addresses on the build host with scripts/profile-resolve.";

struct Options {
    hz: u64,
    duration_s: u64,
    output: Option<String>,
    command: Vec<String>,
}

fn main() {
    let opts = match parse_args() {
        Ok(opts) => opts,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let period_ns = profile::hz_to_period_ns(opts.hz);
    let Some(period) = profile::start(period_ns) else {
        eprintln!("profile: another process holds the profiler");
        std::process::exit(1);
    };

    let mut folded: HashMap<Key, u64> = HashMap::new();
    let mut threads: HashMap<u64, String> = HashMap::new();

    let child = if opts.command.is_empty() {
        None
    } else {
        // `spawn_program_with_fds` is the one that searches for the binary the
        // way the shell does and reports failure as `None`; the raw `spawn`
        // returns a negated errno, which no comparison against a single
        // sentinel catches.
        let args: Vec<String> = opts.command[1..].to_vec();
        match process::spawn_program_with_fds(&opts.command[0], &args, 0, 1, 2) {
            Some(pid) => Some(pid),
            None => {
                profile::stop();
                eprintln!("profile: cannot start {}", opts.command[0]);
                std::process::exit(1);
            }
        }
    };

    collect(&mut folded, &mut threads, &opts, child);

    let stats = profile::stats();
    profile::stop();

    let mut out: Box<dyn Write> = match &opts.output {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => Box::new(file),
            Err(err) => {
                eprintln!("profile: {path}: {err}");
                std::process::exit(1);
            }
        },
        None => Box::new(std::io::stdout()),
    };

    let (taken, dropped) = stats.map(|s| (s.taken, s.dropped)).unwrap_or((0, 0));
    let _ = writeln!(out, "# edos-profile 1");
    let _ = writeln!(out, "# period_ns {period}");
    let _ = writeln!(out, "# taken {taken}");
    let _ = writeln!(out, "# dropped {dropped}");
    let mut tids: Vec<&u64> = threads.keys().collect();
    tids.sort_unstable();
    for tid in tids {
        let _ = writeln!(out, "# thread {tid} {}", threads[tid]);
    }

    // Heaviest first, so the answer is the first line rather than the whole
    // file sorted by something else.
    let mut rows: Vec<(&Key, &u64)> = folded.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (key, count) in rows {
        let _ = writeln!(out, "{} {} {} {count}", key.mode, key.tid, key.stack);
    }

    if dropped > 0 {
        eprintln!(
            "profile: {dropped} samples lost, ring full ({}%)",
            dropped * 100 / (taken + dropped).max(1)
        );
    }
}

/// A folded stack: everything that has to match for two samples to be the same
/// row.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Key {
    mode: char,
    tid: u64,
    stack: String,
}

/// Drain until the run is over, folding as we go.
///
/// Folding here rather than at the end is what keeps the memory bounded: a
/// 30-second run at 999 Hz on 16 CPUs is half a million samples, and the
/// number of distinct stacks in it is a few hundred.
fn collect(
    folded: &mut HashMap<Key, u64>,
    threads: &mut HashMap<u64, String>,
    opts: &Options,
    child: Option<u64>,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(opts.duration_s);
    let mut buf = vec![Sample::zeroed(); 128];
    // Set once the run is over. The loop then keeps folding until a read comes
    // back empty, so the samples taken between the last check and the end are
    // not thrown away.
    let mut draining = false;

    loop {
        let n = profile::read(&mut buf, if draining { 0 } else { 100 });
        for sample in &buf[..n] {
            let mode = if sample.flags & SAMPLE_IDLE != 0 {
                'i'
            } else if sample.is_user() {
                'u'
            } else {
                'k'
            };
            let mut stack = String::new();
            // Outermost first, which is the order a flame graph stacks them.
            for (i, addr) in sample.stack().iter().rev().enumerate() {
                if i > 0 {
                    stack.push(';');
                }
                stack.push_str(&format!("{addr:#x}"));
            }
            // A walk that ended early is a different stack from the same
            // frames walked to the end, and merging them would invent callers.
            if sample.flags & (SAMPLE_BROKEN_CHAIN | SAMPLE_TRUNCATED) != 0 {
                stack.insert_str(0, "[unknown];");
            }
            if sample.tid != 0 {
                threads
                    .entry(sample.tid)
                    .or_insert_with(|| thread_name(sample.tid));
            }
            *folded
                .entry(Key {
                    mode,
                    tid: sample.tid,
                    stack,
                })
                .or_insert(0) += 1;
        }

        if draining && n == 0 {
            return;
        }
        match child {
            // The child is the clock: sample exactly its life, no more. The
            // wait is the non-blocking one because this loop must keep
            // draining; and it is a wait rather than a look at procfs because
            // the entry outlives the process until something reaps it, so a
            // poll on `/proc/<pid>` would never go away.
            Some(pid) => draining |= process::waitpid_nonblocking(pid).is_some(),
            None => draining |= std::time::Instant::now() >= deadline,
        }
    }
}

/// What to call a thread in the header. The command line is per address
/// space, so every thread of a process reports the same one.
fn thread_name(tid: u64) -> String {
    std::fs::read_to_string(format!("/proc/{tid}/cmdline"))
        .map(|s| s.trim().replace('\n', " "))
        .unwrap_or_else(|_| "?".to_string())
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        hz: 999,
        duration_s: 5,
        output: None,
        command: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-f" => {
                let value = args.next().ok_or("profile: -f needs a frequency")?;
                opts.hz = value
                    .parse()
                    .map_err(|_| format!("profile: bad frequency {value}"))?;
            }
            "-d" => {
                let value = args.next().ok_or("profile: -d needs a duration")?;
                opts.duration_s = value
                    .parse()
                    .map_err(|_| format!("profile: bad duration {value}"))?;
            }
            "-o" => {
                opts.output = Some(args.next().ok_or("profile: -o needs a path")?);
            }
            _ => {
                opts.command.push(arg);
                opts.command.extend(args);
                break;
            }
        }
    }
    Ok(opts)
}
