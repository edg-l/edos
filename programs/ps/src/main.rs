//! ps - show process list with color output

use std::fs;
use std::process;

fn main() {
    let text = match fs::read_to_string("/proc/processes") {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ps: failed to read /proc/processes: {}", e);
            process::exit(1);
        }
    };

    for (i, line) in text.lines().enumerate() {
        if i == 0 {
            // Header in bold
            println!("\x1B[1m{}\x1B[0m", line);
            continue;
        }

        // Colorize state column
        let colored = if line.contains("Sleeping") {
            line.replacen("Sleeping", "\x1B[33mSleeping\x1B[0m", 1)
        } else if line.contains("Ready") {
            line.replacen("Ready", "\x1B[32mReady\x1B[0m", 1)
        } else if line.contains("Running") {
            line.replacen("Running", "\x1B[1;32mRunning\x1B[0m", 1)
        } else if line.contains("Parked") {
            line.replacen("Parked", "\x1B[34mParked\x1B[0m", 1)
        } else {
            line.to_string()
        };
        println!("{}", colored);
    }
}
