//! mkdir - create a directory

use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: mkdir <path>");
        process::exit(1);
    }

    let path = &args[1];
    if let Err(e) = fs::create_dir(path) {
        eprintln!("mkdir: {}: {}", path, e);
        process::exit(1);
    }
}
