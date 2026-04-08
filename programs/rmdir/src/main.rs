//! rmdir - remove an empty directory

use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: rmdir <path>");
        process::exit(1);
    }

    let path = &args[1];
    if let Err(e) = fs::remove_dir(path) {
        eprintln!("rmdir: {}: {}", path, e);
        process::exit(1);
    }
}
