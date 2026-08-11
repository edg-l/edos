//! Print the number of processing units available.
//!
//! Read from `/proc/cpuinfo`, which reports both what the kernel detected in
//! the ACPI tables and how many of those actually came up. The two differ when
//! an AP fails to start, and the useful answer for anything sizing a thread
//! pool is the online count, so that is the default; `--all` asks for the
//! other one.

use std::{fs, process::ExitCode};

const USAGE: &str = "usage: nproc [--all]";

fn main() -> ExitCode {
    let mut want_detected = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--all" => want_detected = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("nproc: unknown option '{other}'\n{USAGE}");
                return ExitCode::from(1);
            }
        }
    }

    let text = match fs::read_to_string("/proc/cpuinfo") {
        Ok(text) => text,
        Err(err) => {
            eprintln!("nproc: /proc/cpuinfo: {err}");
            return ExitCode::from(1);
        }
    };

    let wanted = if want_detected {
        "cpus detected:"
    } else {
        "cpus online:"
    };
    let Some(count) = text
        .lines()
        .find_map(|line| line.strip_prefix(wanted))
        .and_then(|value| value.trim().parse::<u64>().ok())
    else {
        eprintln!("nproc: /proc/cpuinfo has no '{wanted}' line");
        return ExitCode::from(1);
    };

    println!("{count}");
    ExitCode::SUCCESS
}
