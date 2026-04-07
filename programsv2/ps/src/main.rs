//! ps - show process list

use std::fs;
use std::process;

fn main() {
    match fs::read_to_string("/proc/processes") {
        Ok(text) => print!("{}", text),
        Err(e) => {
            eprintln!("ps: failed to read /proc/processes: {}", e);
            process::exit(1);
        }
    }
}
