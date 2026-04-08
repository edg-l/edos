//! write - write or append text to a file

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: write <path> <content...>");
        process::exit(1);
    }

    let path = &args[1];
    let content = args[2..].join(" ");

    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("write: {}: {}", path, e);
            process::exit(1);
        }
    };

    if let Err(e) = writeln!(file, "{}", content) {
        eprintln!("write: {}: {}", path, e);
        process::exit(1);
    }
}
