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

    let file_type = if meta.is_dir() {
        "directory"
    } else if meta.is_symlink() {
        "symbolic link"
    } else {
        "regular file"
    };

    println!("  File: {}", path);
    println!("  Type: {}", file_type);
    println!("  Size: {}", meta.len());
}
