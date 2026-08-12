//! Drive every syscall the kernel publishes with arguments no caller would
//! send: unmapped pointers, absurd lengths, misaligned structs, file
//! descriptors that were never opened.
//!
//! Nothing else tests the `uaccess` surface *as a surface*. Each wrapper in
//! `edos_lib` passes arguments a correct program built, so the only paths ever
//! exercised are the ones where the pointer is valid and the length fits. A
//! syscall that reads through a pointer without validating it corrupts the
//! kernel from userspace, and a dispatch arm that trusts a length walks off
//! the end of a buffer; both are invisible until something sends the bad
//! argument on purpose.
//!
//! The argument shapes come from `/proc/syscalls`, which names every call and
//! says which of its arguments are pointers, lengths, descriptors and strings.
//! There is no table here to drift out of step with the kernel's: a syscall
//! added to `kernel/src/syscalls/table.rs` is fuzzed the next time this runs.
//!
//! A correct kernel answers every *poisoned* case with a failure, so the report
//! is a tally of which failure. The interesting rows are the calls that
//! *succeeded* on garbage, and the last line printed before a hang or a panic
//! names the call that caused it.
//!
//! Not every generated case is poisoned. A pointer argument is sometimes the
//! valid scratch buffer and a length is sometimes 0 or 1, on purpose: without
//! them the kernel's own checks are short-circuited by an obviously bad address
//! and the code past them is never reached. Those cases are counted as `benign`
//! and cannot be findings, because the syscall was asked a question it should
//! answer. Only a case with at least one poisoned argument is reported as
//! having returned rather than failed.
//!
//! Cases are deterministic: the generator is seeded with `seed ^ nr`, so
//! `--only <name>` reproduces exactly the cases that name got in the full run.

use std::env;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::process;

use edos_lib::sys::{
    SYS_ERRNO, syscall0, syscall1, syscall2, syscall3, syscall4, syscall5, syscall6,
};
use edos_lib::trace::{ArgKind, SyscallTable, read_syscall_table};

/// Calls this refuses to make, and why.
///
/// Two reasons only: it would not return, or its side effect would outlive the
/// fuzzer. Everything else is fair game, including the whole path-resolution
/// and socket surface, because the descriptors and pointers handed to those
/// are invalid by construction.
const SKIPPED: &[(&str, &str)] = &[
    ("exit", "ends the fuzzer"),
    ("execve", "replaces the fuzzer's image"),
    (
        "fork",
        "a forked child would fuzz in parallel and race the report",
    ),
    (
        "clone",
        "same, and a garbage entry point faults in an unnamed thread",
    ),
    ("spawn", "leaves a process behind"),
    ("spawn2", "leaves a process behind"),
    ("sigreturn", "unwinds onto a frame that was never pushed"),
    ("kill", "pid -1 reaches every process, init included"),
    ("reboot", "ends the machine"),
    (
        "mount",
        "would alter the namespace the rest of the run walks",
    ),
    ("sync", "the journal-drain path can take 30 s per call"),
    ("poll", "a valid pointer with a huge timeout parks forever"),
    ("nanosleep", "a garbage timespec parks for years"),
    ("sleep_ms", "same"),
    ("futex_wait", "parks with nothing to wake it"),
    ("waitpid", "parks on a child that does not exist"),
    ("unlink", "deletes"),
    ("unlinkat", "deletes"),
    ("rmdir", "deletes"),
    ("rmdir_all", "deletes a tree"),
    ("rename", "moves"),
    ("renameat", "moves"),
    ("truncate", "discards a file's tail by path"),
    (
        "clock_settime",
        "moves the wall clock under everything else running",
    ),
    (
        "setpgid",
        "detaches the fuzzer from the shell's job control",
    ),
    (
        "tcsetpgrp",
        "hands the terminal to a process group that may not exist",
    ),
    ("sigaction", "installs a handler at a garbage address"),
    ("sigprocmask", "can block the signal that stops the fuzzer"),
    (
        "shm_destroy",
        "segment ids are global, so a guess hits another process",
    ),
    ("trace_ctl", "claims the single tracer session"),
    ("window_create", "puts a window over the desktop"),
    ("window_destroy", "window ids are global"),
    ("window_set", "window ids are global"),
    ("window_damage", "window ids are global"),
    (
        "window_send_event",
        "delivers a forged event to the compositor",
    ),
    (
        "window_grant_shell",
        "grants shell rights to whatever id it guesses",
    ),
];

/// Addresses chosen to be unmapped in *this* process: below the image, in the
/// hole between the heap and the 8 MiB stack that tops out at
/// `0x0000_7000_0000_0000`, in kernel space, and non-canonical.
const POISON_PTRS: &[u64] = &[
    0,
    1,
    0x1000,
    0x1001,
    0x0000_5000_0000_0000,
    0x0000_6fff_0000_0000,
    0x0000_8000_0000_0000,
    0xffff_8000_0000_0000,
    0xffff_ffff_8000_0000,
    u64::MAX,
];

/// A set of scalar arguments, ordered so that the values a correct caller could
/// have sent come first. Anything past `plausible` is poison, and only a case
/// built from at least one poison argument can be a finding.
struct Values {
    all: &'static [u64],
    plausible: usize,
}

/// Descriptors that are never open here. 0, 1 and 2 are deliberately absent:
/// closing or redirecting them would take the report with them. None of these
/// is plausible, which is the point of the set.
const POISON_FDS: Values = Values {
    all: &[u64::MAX, (-100i64) as u64, 9999, 0x7fff_ffff, 0x1_0000_0000],
    plausible: 0,
};

/// 0 and 1 name the caller's own group, pid 1, the first index — a correct
/// program sends them constantly.
const POISON_INTS: Values = Values {
    all: &[
        0,
        1,
        (-1i64) as u64,
        (i32::MIN as i64) as u64,
        i64::MIN as u64,
        i64::MAX as u64,
    ],
    plausible: 2,
};

/// Flag and mode words: no flags, and a mode `mkdir` is given every day.
const POISON_HEX: Values = Values {
    all: &[0, 0o777, 0xffff_ffff, 0x8000_0000, u64::MAX],
    plausible: 2,
};

/// A zero or one-byte transfer fits the scratch buffer; 4097 is one past its
/// end on purpose.
const POISON_LENS: Values = Values {
    all: &[0, 1, 4097, 0xffff_ffff, i64::MAX as u64, u64::MAX],
    plausible: 2,
};

/// A path that does not exist. `open` with every flag bit set includes
/// `O_CREAT`, so this is a name the fuzzer is willing to have created; it is
/// removed again on the way out.
const PROBE_PATH: &str = "/var/.syscallfuzz-probe";

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*: small, and reproducible from the printed seed.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn pick(&mut self, xs: &[u64]) -> u64 {
        xs[(self.next() % xs.len() as u64) as usize]
    }

    /// Pick from a set, saying whether the value picked was poison.
    fn pick_from(&mut self, set: &Values) -> Arg {
        let index = (self.next() % set.all.len() as u64) as usize;
        Arg {
            value: set.all[index],
            poison: index >= set.plausible,
        }
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Buffers a pointer argument can legitimately name, so the kernel's length
/// checks are reached rather than short-circuited by an obviously bad address.
struct Valid {
    scratch: Vec<u8>,
    path: Vec<u8>,
    unterminated: Vec<u8>,
}

impl Valid {
    fn new() -> Self {
        let mut path = PROBE_PATH.as_bytes().to_vec();
        path.push(0);
        Self {
            scratch: vec![0u8; 4096],
            path,
            // No NUL anywhere in it: a string argument pointed here has to be
            // rejected by the kernel's own scan limit.
            unterminated: vec![b'A'; 4096],
        }
    }
}

/// One generated argument, and whether it is something a correct caller would
/// never have sent.
struct Arg {
    value: u64,
    poison: bool,
}

impl Arg {
    fn poison(value: u64) -> Self {
        Self {
            value,
            poison: true,
        }
    }

    fn plausible(value: u64) -> Self {
        Self {
            value,
            poison: false,
        }
    }
}

/// One argument, generated for its declared kind.
fn arg_for(kind: ArgKind, rng: &mut Rng, valid: &mut Valid) -> Arg {
    match kind {
        ArgKind::Fd => rng.pick_from(&POISON_FDS),
        ArgKind::Int => rng.pick_from(&POISON_INTS),
        ArgKind::Hex => rng.pick_from(&POISON_HEX),
        ArgKind::Len => rng.pick_from(&POISON_LENS),
        ArgKind::Str => match rng.below(4) {
            0 => Arg::plausible(valid.path.as_ptr() as u64),
            1 => Arg::poison(valid.unterminated.as_ptr() as u64),
            _ => Arg::poison(rng.pick(POISON_PTRS)),
        },
        ArgKind::Ptr | ArgKind::Buf | ArgKind::Out | ArgKind::StrLen => match rng.below(4) {
            0 => Arg::plausible(valid.scratch.as_mut_ptr() as u64),
            // Deliberately off by one, so a struct argument arrives misaligned.
            1 => Arg::poison(valid.scratch.as_mut_ptr() as u64 + 1),
            _ => Arg::poison(rng.pick(POISON_PTRS)),
        },
    }
}

fn invoke(nr: u64, args: &[u64]) -> u64 {
    unsafe {
        match args.len() {
            0 => syscall0(nr),
            1 => syscall1(nr, args[0]),
            2 => syscall2(nr, args[0], args[1]),
            3 => syscall3(nr, args[0], args[1], args[2]),
            4 => syscall4(nr, args[0], args[1], args[2], args[3]),
            5 => syscall5(nr, args[0], args[1], args[2], args[3], args[4]),
            _ => syscall6(nr, args[0], args[1], args[2], args[3], args[4], args[5]),
        }
    }
}

fn errno() -> u32 {
    unsafe { syscall0(SYS_ERRNO) as u32 }
}

/// Failure convention: the syscall returns `u64::MAX` and leaves the reason in
/// the thread's errno slot.
const FAILED: u64 = u64::MAX;

#[derive(Default)]
struct Tally {
    calls: u64,
    /// Errno value to how many calls answered with it.
    errnos: Vec<(u32, u64)>,
    /// Poisoned cases that returned something other than a failure, by case
    /// index.
    returned: Vec<usize>,
    /// Cases that succeeded with no poisoned argument at all. They are the
    /// point of generating plausible values — they reach the code past the
    /// kernel's argument checks — and succeeding is what they should do.
    benign: u64,
}

impl Tally {
    fn record_errno(&mut self, value: u32) {
        match self.errnos.iter_mut().find(|(v, _)| *v == value) {
            Some((_, count)) => *count += 1,
            None => self.errnos.push((value, 1)),
        }
    }

    fn render(&self, table: &SyscallTable) -> String {
        let mut out = String::new();
        let mut errnos = self.errnos.clone();
        errnos.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        for (value, count) in errnos {
            let name = table
                .errno_name(value)
                .map(str::to_string)
                .unwrap_or_else(|| format!("errno{value}"));
            let _ = write!(out, " {name}x{count}");
        }
        if self.benign > 0 {
            let _ = write!(out, " benign={}", self.benign);
        }
        if !self.returned.is_empty() {
            let _ = write!(out, " returned={}", self.returned.len());
        }
        out
    }
}

fn kinds_of(args: &[ArgKind]) -> String {
    if args.is_empty() {
        "-".to_string()
    } else {
        args.iter().map(|k| k.as_char()).collect()
    }
}

fn usage() -> ! {
    eprintln!(
        "Usage: syscallfuzz [OPTIONS]

Drives every syscall in /proc/syscalls with invalid arguments.

Options:
  -s, --seed N     seed the case generator (default 1)
  -n, --cases N    argument combinations per syscall (default 24)
  -o, --only NAME  fuzz only this syscall, with the cases it gets in a full run
  -u, --unknown N  also call N numbers absent from the table (default 32)
  -l, --list       print what would be fuzzed and what is skipped, and exit
  -v, --verbose    print every case and its result"
    );
    process::exit(1);
}

struct Opts {
    seed: u64,
    cases: usize,
    only: Option<String>,
    unknown: u64,
    list: bool,
    verbose: bool,
}

fn parse_args() -> Opts {
    let mut opts = Opts {
        seed: 1,
        cases: 24,
        only: None,
        unknown: 32,
        list: false,
        verbose: false,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> String {
            args.next().unwrap_or_else(|| {
                eprintln!("syscallfuzz: {name} needs a value");
                process::exit(1);
            })
        };
        match arg.as_str() {
            "-s" | "--seed" => opts.seed = value("--seed").parse().unwrap_or_else(|_| usage()),
            "-n" | "--cases" => opts.cases = value("--cases").parse().unwrap_or_else(|_| usage()),
            "-o" | "--only" => opts.only = Some(value("--only")),
            "-u" | "--unknown" => {
                opts.unknown = value("--unknown").parse().unwrap_or_else(|_| usage())
            }
            "-l" | "--list" => opts.list = true,
            "-v" | "--verbose" => opts.verbose = true,
            "-h" | "--help" => usage(),
            other => {
                eprintln!("syscallfuzz: unknown option {other}");
                usage();
            }
        }
    }
    opts
}

fn skip_reason(name: &str) -> Option<&'static str> {
    SKIPPED
        .iter()
        .find(|(skipped, _)| *skipped == name)
        .map(|(_, reason)| *reason)
}

fn main() {
    let opts = parse_args();
    let table = read_syscall_table();
    if table.calls.is_empty() {
        eprintln!("syscallfuzz: /proc/syscalls is empty; is procfs mounted?");
        process::exit(1);
    }

    if opts.list {
        for (nr, info) in &table.calls {
            match skip_reason(&info.name) {
                Some(reason) => println!("skip {nr:5} {:<20} {reason}", info.name),
                None => println!("fuzz {nr:5} {:<20} {}", info.name, kinds_of(&info.args)),
            }
        }
        return;
    }

    let mut valid = Valid::new();
    let mut fuzzed = 0u64;
    let mut skipped = 0u64;
    let mut total_calls = 0u64;
    let mut total = Tally::default();
    let mut returned: Vec<String> = Vec::new();

    println!(
        "syscallfuzz: seed {}, {} cases per call, {} calls in /proc/syscalls",
        opts.seed,
        opts.cases,
        table.calls.len()
    );

    for (nr, info) in &table.calls {
        if let Some(only) = &opts.only {
            if &info.name != only {
                continue;
            }
        } else if let Some(reason) = skip_reason(&info.name) {
            skipped += 1;
            if opts.verbose {
                println!("  {:<18} skipped: {reason}", info.name);
            }
            continue;
        }

        // Printed before the first case, without a newline, so the call that
        // hangs or panics the kernel is the last thing on screen.
        print!("  {:<18} {:<7}", info.name, kinds_of(&info.args));
        if opts.verbose {
            println!();
        }
        let _ = io::stdout().flush();

        let nr = *nr as u64;
        let mut rng = Rng(opts.seed ^ nr ^ 0x9e37_79b9_7f4a_7c15);
        let mut tally = Tally::default();
        let mut args = vec![0u64; info.args.len()];
        let mut first_returned = String::new();
        for case in 0..opts.cases {
            // A call with no arguments cannot be sent a bad one, so whatever it
            // answers is the correct answer rather than a finding.
            let mut poisoned = false;
            for (slot, kind) in args.iter_mut().zip(&info.args) {
                let arg = arg_for(*kind, &mut rng, &mut valid);
                *slot = arg.value;
                poisoned |= arg.poison;
            }
            if opts.verbose {
                // Before the call, not after: a case that never returns is
                // exactly the one worth naming.
                let mark = if poisoned { "" } else { " benign" };
                print!("    case {case:3}{mark} {args:x?} -> ");
                let _ = io::stdout().flush();
            }
            let ret = invoke(nr, &args);
            tally.calls += 1;
            if ret == FAILED {
                tally.record_errno(errno());
            } else if poisoned {
                if tally.returned.is_empty() {
                    first_returned = format!("{args:x?}");
                }
                tally.returned.push(case);
            } else {
                tally.benign += 1;
            }
            if opts.verbose {
                println!("{ret:#x}");
            }
        }

        if !tally.returned.is_empty() {
            returned.push(format!(
                "{} answered {} on case {}; reproduce with --seed {} --only {}",
                info.name, first_returned, tally.returned[0], opts.seed, info.name
            ));
        }
        if opts.verbose {
            print!("  {:<18} {:<7}", info.name, kinds_of(&info.args));
        }
        println!(" {:4} calls{}", tally.calls, tally.render(&table));
        total_calls += tally.calls;
        for (value, count) in &tally.errnos {
            for _ in 0..*count {
                total.record_errno(*value);
            }
        }
        total.benign += tally.benign;
        fuzzed += 1;
    }

    let mut unknown_rejected = 0u64;
    let mut unknown_answered: Vec<u64> = Vec::new();
    if opts.only.is_none() && opts.unknown > 0 {
        // Numbers the table does not list must reach the dispatcher's default
        // arm. One that answers instead is an arm `table.rs` forgot, which is
        // also what makes strace print `syscall_<n>`.
        let mut rng = Rng(opts.seed ^ 0xfeed_face_cafe_babe);
        let mut tried = 0u64;
        while tried < opts.unknown {
            let nr = rng.below(0x600);
            if table.calls.contains_key(&(nr as u32)) {
                continue;
            }
            tried += 1;
            print!("  unknown {nr:<10}    ");
            let _ = io::stdout().flush();
            let ret = invoke(nr, &[0x0000_5000_0000_0000; 6]);
            total_calls += 1;
            if ret == FAILED {
                unknown_rejected += 1;
                let value = errno();
                total.record_errno(value);
                println!("rejected");
            } else {
                unknown_answered.push(nr);
                println!("ANSWERED {ret:#x}");
            }
        }
    }

    // `open` with every flag bit set includes O_CREAT, so the probe path may
    // exist now.
    let _ = std::fs::remove_file(PROBE_PATH);

    println!("--- summary ---");
    println!("{fuzzed} syscalls fuzzed, {skipped} skipped, {total_calls} calls made");
    println!("failures:{}", total.render(&table));
    if opts.unknown > 0 && opts.only.is_none() {
        println!(
            "unknown numbers: {unknown_rejected} rejected, {} answered",
            unknown_answered.len()
        );
        for nr in &unknown_answered {
            println!("  {nr} dispatches but is absent from /proc/syscalls");
        }
    }
    if !returned.is_empty() {
        println!(
            "poisoned yet returned rather than failed, {} call(s):",
            returned.len()
        );
        for line in &returned {
            println!("  {line}");
        }
    }
    if unknown_answered.is_empty() {
        println!("no unlisted dispatch arm, no kernel fault: PASS");
    } else {
        // A number that dispatches without being in the table means `strace`
        // cannot name it and nothing describes its arguments.
        process::exit(1);
    }
}
