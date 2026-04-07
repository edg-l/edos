//! mv - move or rename a file or directory

use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: mv <source> <dest>");
        process::exit(1);
    }

    let source = &args[1];
    let dest = &args[2];

    if let Err(e) = fs::rename(source, dest) {
        eprintln!("mv: {} -> {}: {}", source, dest, e);
        process::exit(1);
    }
}
