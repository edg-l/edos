//! `strace`: what a process actually asked the kernel for.
//!
//! The kernel records syscall entries and returns for marked threads into a
//! ring; this drains it and prints one line per completed call. Children of a
//! traced process are traced too, so a shell script under trace shows the
//! programs it runs.
//!
//! The tracer never stops the target. A target that outruns the reader loses
//! records, and the count of what was lost is printed at the end rather than
//! left to be guessed at.

mod format;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;

use edos_lib::process;
use edos_lib::trace::{self, SyscallTable, TRACE_DIED, TRACE_ENTER, TRACE_EXIT, TraceRecord};

const USAGE: &str = "\
usage: strace [options] PROGRAM [ARGS...]
       strace [options] -p PID

  -p PID     trace a running process instead of starting one
  -o FILE    write the trace to FILE instead of stderr
  -e LIST    only show these syscalls (comma separated)
  -s SIZE    show at most SIZE bytes of each string (default 32)
  -c         print a summary per syscall instead of a line per call
  -T         show how long each call took
  -h         this message

Children of a traced process are traced as well.";

struct Options {
    attach: Option<u64>,
    output: Option<String>,
    filter: HashSet<String>,
    max_str: usize,
    summary: bool,
    show_time: bool,
    command: Vec<String>,
}

/// A call whose return has not arrived yet.
struct Pending {
    enter: TraceRecord,
    /// Set once the call has been reported as `<unfinished ...>`, so its
    /// return prints as a resumption rather than a second full line.
    reported: bool,
}

#[derive(Default)]
struct CallStats {
    calls: u64,
    errors: u64,
    time_ns: u64,
}

fn main() {
    let opts = match parse_args() {
        Ok(opts) => opts,
        Err(message) => {
            eprintln!("strace: {message}\n{USAGE}");
            std::process::exit(2);
        }
    };

    let table = trace::read_syscall_table();
    if table.calls.is_empty() {
        eprintln!("strace: /proc/syscalls is unreadable; is procfs mounted on /proc?");
        std::process::exit(1);
    }

    let mut out: Box<dyn Write> = match &opts.output {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => Box::new(file),
            Err(err) => {
                eprintln!("strace: cannot write {path}: {err}");
                std::process::exit(1);
            }
        },
        None => Box::new(std::io::stderr()),
    };

    if !trace::claim() {
        eprintln!("strace: another tracer already holds the kernel trace ring");
        std::process::exit(1);
    }

    let (root, spawned) = match opts.attach {
        Some(pid) => {
            if !trace::mark(pid) {
                trace::release();
                eprintln!("strace: no such process: {pid}");
                std::process::exit(1);
            }
            (pid, false)
        }
        None => (spawn_traced(&opts.command), true),
    };

    let stats = follow(&table, &opts, root, spawned, out.as_mut());

    trace::release();

    if opts.summary {
        print_summary(&table, out.as_mut(), &stats);
    }

    let dropped = trace::dropped();
    if dropped > 0 {
        let _ = writeln!(
            out,
            "strace: {dropped} records lost (the trace ring filled up)"
        );
    }

    // The trace already reported the exit status; this only collects the
    // record so the child is not left for init to reap.
    if spawned {
        process::waitpid(root);
    }
}

/// Fork, mark the child, and replace it with the traced program.
///
/// Marking from inside the child rather than attaching to it from here is what
/// makes the first call it ever issues visible: there is no window in which it
/// is running and not yet marked.
fn spawn_traced(command: &[String]) -> u64 {
    let path = if command[0].contains('/') {
        command[0].clone()
    } else {
        format!("/bin/{}", command[0])
    };

    let Ok(pid) = process::fork() else {
        trace::release();
        eprintln!("strace: fork failed");
        std::process::exit(1);
    };

    if pid == 0 {
        trace::mark(0);
        let args: Vec<&str> = command[1..].iter().map(|s| s.as_str()).collect();
        let env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
        let env_refs: Vec<&str> = env.iter().map(|s| s.as_str()).collect();
        let e = process::execve(&path, &args, &env_refs);
        eprintln!("strace: cannot run {path}: {e:?}");
        std::process::exit(127);
    }

    pid
}

/// Drain records until every traced thread has died.
fn follow(
    table: &SyscallTable,
    opts: &Options,
    root: u64,
    spawned: bool,
    out: &mut dyn Write,
) -> BTreeMap<u32, CallStats> {
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let mut live: HashSet<u64> = HashSet::from([root]);
    let mut stats: BTreeMap<u32, CallStats> = BTreeMap::new();
    let mut buf = vec![TraceRecord::zeroed(); 256];

    loop {
        // Once nothing is left alive the remaining records are already in the
        // ring, so stop parking for more.
        let timeout = if live.is_empty() { 0 } else { 200 };
        let count = trace::read(&mut buf, timeout);

        if count == 0 {
            // The death record is what normally ends this, but it travels
            // through a ring that can drop under load — and a target fast
            // enough to overflow the ring is exactly the one whose exit gets
            // lost. Reaping the child is a second, unloseable signal.
            if live.is_empty() || (spawned && process::waitpid_nonblocking(root).is_some()) {
                break;
            }
            // Everything traced is blocked in a call: say which, so a process
            // that appears to be doing nothing shows what it is waiting on.
            report_unfinished(table, opts, root, &mut pending, out);
            continue;
        }

        for record in &buf[..count] {
            live.insert(record.tid);
            match record.kind {
                TRACE_ENTER => {
                    pending.insert(
                        record.tid,
                        Pending {
                            enter: *record,
                            reported: false,
                        },
                    );
                }
                TRACE_EXIT => {
                    // Counted only once the entry is in hand, so a return
                    // whose entry was lost cannot inflate the call count while
                    // contributing no time to it.
                    let Some(call) = pending.remove(&record.tid) else {
                        continue;
                    };
                    let entry = stats.entry(record.nr).or_default();
                    entry.calls += 1;
                    if (record.ret as i64) < 0 {
                        entry.errors += 1;
                    }
                    entry.time_ns += record.time_ns.saturating_sub(call.enter.time_ns);

                    if opts.summary || !shown(table, opts, record.nr) {
                        continue;
                    }
                    let line = if call.reported {
                        format::format_resumed(table, record)
                    } else {
                        format::format_call(table, &call.enter, record, opts.max_str)
                    };
                    emit(
                        out,
                        root,
                        record.tid,
                        &line,
                        elapsed(opts, &call.enter, record),
                    );
                }
                TRACE_DIED => {
                    live.remove(&record.tid);
                    // `exit` never returns, and neither does a call the thread
                    // was killed inside, so the entry record is all there will
                    // ever be for it.
                    if let Some(call) = pending.remove(&record.tid)
                        && !opts.summary
                        && !call.reported
                        && shown(table, opts, call.enter.nr)
                    {
                        let line = format::format_abandoned(table, &call.enter, opts.max_str);
                        emit(out, root, record.tid, &line, None);
                    }
                    if !opts.summary {
                        let line = format!("+++ exited with {} +++", record.ret as i64);
                        emit(out, root, record.tid, &line, None);
                    }
                }
                _ => {}
            }
        }
    }

    stats
}

/// Print every in-flight call that has not been reported yet.
fn report_unfinished(
    table: &SyscallTable,
    opts: &Options,
    root: u64,
    pending: &mut HashMap<u64, Pending>,
    out: &mut dyn Write,
) {
    if opts.summary {
        return;
    }
    for (tid, call) in pending.iter_mut() {
        if call.reported || !shown(table, opts, call.enter.nr) {
            continue;
        }
        call.reported = true;
        let line = format::format_unfinished(table, &call.enter, opts.max_str);
        emit(out, root, *tid, &line, None);
    }
}

fn shown(table: &SyscallTable, opts: &Options, nr: u32) -> bool {
    opts.filter.is_empty() || opts.filter.contains(&table.name_of(nr))
}

fn elapsed(opts: &Options, enter: &TraceRecord, exit: &TraceRecord) -> Option<u64> {
    opts.show_time
        .then(|| exit.time_ns.saturating_sub(enter.time_ns))
}

/// One trace line. The tid is shown only for threads other than the one asked
/// for, which keeps the single-process case unadorned.
fn emit(out: &mut dyn Write, root: u64, tid: u64, line: &str, took_ns: Option<u64>) {
    let prefix = if tid == root {
        String::new()
    } else {
        format!("[pid {tid}] ")
    };
    match took_ns {
        Some(ns) => {
            let _ = writeln!(
                out,
                "{prefix}{line} <{}.{:06}>",
                ns / 1_000_000_000,
                (ns % 1_000_000_000) / 1_000
            );
        }
        None => {
            let _ = writeln!(out, "{prefix}{line}");
        }
    }
}

fn print_summary(table: &SyscallTable, out: &mut dyn Write, stats: &BTreeMap<u32, CallStats>) {
    let mut rows: Vec<(&u32, &CallStats)> = stats.iter().collect();
    rows.sort_by_key(|(_, stat)| core::cmp::Reverse(stat.time_ns));

    let total_ns: u64 = rows.iter().map(|(_, stat)| stat.time_ns).sum();
    let total_calls: u64 = rows.iter().map(|(_, stat)| stat.calls).sum();

    let _ = writeln!(
        out,
        "{:>8} {:>10} {:>8} {:>7} syscall",
        "% time", "usecs", "calls", "errors"
    );
    for (nr, stat) in &rows {
        let share = if total_ns == 0 {
            0.0
        } else {
            stat.time_ns as f64 * 100.0 / total_ns as f64
        };
        let _ = writeln!(
            out,
            "{share:>7.2}% {:>10} {:>8} {:>7} {}",
            stat.time_ns / 1_000,
            stat.calls,
            stat.errors,
            table.name_of(**nr)
        );
    }
    let _ = writeln!(
        out,
        "{:>8} {:>10} {:>8} {:>7} total",
        "100.00%",
        total_ns / 1_000,
        total_calls,
        rows.iter().map(|(_, s)| s.errors).sum::<u64>()
    );
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        attach: None,
        output: None,
        filter: HashSet::new(),
        max_str: 32,
        summary: false,
        show_time: false,
        command: Vec::new(),
    };

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let value = |i: usize, name: &str| -> Result<String, String> {
        argv.get(i + 1)
            .cloned()
            .ok_or_else(|| format!("{name} needs an argument"))
    };

    let mut i = 0;
    while i < argv.len() {
        let mut consumed = 2;
        match argv[i].as_str() {
            "-p" => {
                let pid = value(i, "-p")?;
                opts.attach = Some(pid.parse().map_err(|_| format!("bad pid: {pid}"))?);
            }
            "-o" => opts.output = Some(value(i, "-o")?),
            "-e" => {
                opts.filter
                    .extend(value(i, "-e")?.split(',').map(|s| s.to_string()));
            }
            "-s" => {
                let size = value(i, "-s")?;
                opts.max_str = size.parse().map_err(|_| format!("bad size: {size}"))?;
            }
            "-c" => {
                opts.summary = true;
                consumed = 1;
            }
            "-T" => {
                opts.show_time = true;
                consumed = 1;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => {
                opts.command = argv[i..].to_vec();
                break;
            }
        }
        i += consumed;
    }

    match (&opts.attach, opts.command.is_empty()) {
        (Some(_), false) => Err("give either -p or a program, not both".to_string()),
        (None, true) => Err("nothing to trace".to_string()),
        _ => Ok(opts),
    }
}
