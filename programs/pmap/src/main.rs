//! pmap - the address space of a running thread
//!
//! `/proc/<tid>/status` counts VMAs and totals their size; it cannot say what
//! any one of them is. This prints the list the kernel keeps: where each
//! mapping starts, how much was asked for, how much of it the machine has
//! actually spent, and what backs it. The gap between the last two columns is
//! demand paging, which is most of how this kernel maps anything.

use std::fs;
use std::process::ExitCode;

/// One line of `/proc/<tid>/maps`.
struct Mapping {
    start: u64,
    end: u64,
    mode: String,
    kbytes: u64,
    rss_kib: u64,
    backing: String,
}

struct Options {
    /// Print the end address as well as the start.
    extended: bool,
    /// Neither header nor totals: just the mappings.
    quiet: bool,
}

fn usage() -> ! {
    eprintln!("usage: pmap [-x] [-q] PID...");
    std::process::exit(2)
}

fn parse_mapping(line: &str) -> Option<Mapping> {
    let mut fields = line.split_whitespace();
    let range = fields.next()?;
    let mode = fields.next()?.to_string();
    let kbytes = fields.next()?.parse().ok()?;
    let rss_kib = fields.next()?.parse().ok()?;
    // A backing token never contains a space, but taking the rest of the line
    // keeps this working if one ever grows a descriptive suffix.
    let backing = fields.collect::<Vec<_>>().join(" ");
    let (start, end) = range.split_once('-')?;
    Some(Mapping {
        start: u64::from_str_radix(start, 16).ok()?,
        end: u64::from_str_radix(end, 16).ok()?,
        mode,
        kbytes,
        rss_kib,
        backing,
    })
}

/// The thread's name, or `?` if it exited between the two reads.
fn thread_name(pid: u64) -> String {
    match fs::read_to_string(format!("/proc/{pid}/cmdline")) {
        Ok(text) => text.trim().to_string(),
        Err(_) => "?".to_string(),
    }
}

fn print_process(pid: u64, options: &Options) -> Result<(), String> {
    let text = fs::read_to_string(format!("/proc/{pid}/maps"))
        .map_err(|err| format!("pmap: {pid}: {err}"))?;

    let mappings: Vec<Mapping> = text
        .lines()
        .skip(1) // the kernel's own column header
        .filter_map(parse_mapping)
        .collect();

    if !options.quiet {
        println!("{pid}:   {}", thread_name(pid));
        if options.extended {
            println!("Address          End              Kbytes    RSS Mode Mapping");
        } else {
            println!("Address          Kbytes    RSS Mode Mapping");
        }
    }

    for map in &mappings {
        if options.extended {
            println!(
                "{:016x} {:016x} {:>6} {:>6} {:<4} {}",
                map.start, map.end, map.kbytes, map.rss_kib, map.mode, map.backing
            );
        } else {
            println!(
                "{:016x} {:>6} {:>6} {:<4} {}",
                map.start, map.kbytes, map.rss_kib, map.mode, map.backing
            );
        }
    }

    if !options.quiet {
        let kbytes: u64 = mappings.iter().map(|map| map.kbytes).sum();
        let rss_kib: u64 = mappings.iter().map(|map| map.rss_kib).sum();
        println!(
            "total {} mappings, {kbytes}K mapped, {rss_kib}K resident",
            mappings.len()
        );
    }

    Ok(())
}

fn main() -> ExitCode {
    let mut options = Options {
        extended: false,
        quiet: false,
    };
    let mut pids: Vec<u64> = Vec::new();

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-x" => options.extended = true,
            "-q" => options.quiet = true,
            "-h" | "--help" => usage(),
            _ => match arg.parse::<u64>() {
                Ok(pid) => pids.push(pid),
                Err(_) => {
                    eprintln!("pmap: not a pid: {arg}");
                    return ExitCode::from(2);
                }
            },
        }
    }

    if pids.is_empty() {
        usage();
    }

    let mut status = ExitCode::SUCCESS;
    for (index, pid) in pids.iter().enumerate() {
        if index > 0 && !options.quiet {
            println!();
        }
        if let Err(message) = print_process(*pid, &options) {
            eprintln!("{message}");
            status = ExitCode::FAILURE;
        }
    }
    status
}
