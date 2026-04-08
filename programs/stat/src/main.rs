//! stat - show file metadata

use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: stat <path>");
        process::exit(1);
    }

    let path = &args[1];
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("stat: {}: {}", path, e);
            process::exit(1);
        }
    };

    let (file_type, type_color) = if meta.is_dir() {
        ("directory", "\x1B[1;34m")
    } else if meta.is_symlink() {
        ("symbolic link", "\x1B[1;36m")
    } else {
        ("regular file", "\x1B[0m")
    };

    println!("  File: \x1B[1m{}\x1B[0m", path);
    println!("  Type: {}{}\x1B[0m", type_color, file_type);
    println!("  Size: \x1B[33m{}\x1B[0m bytes", meta.len());
}
