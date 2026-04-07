//! rm - remove file or directory

use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let args = &args[1..];

    let recursive = args.iter().any(|a| a == "-r");
    let paths: Vec<&str> = args
        .iter()
        .filter(|a| *a != "-r")
        .map(|s| s.as_str())
        .collect();

    if paths.is_empty() {
        eprintln!("Usage: rm [-r] <path>");
        process::exit(1);
    }

    let mut had_error = false;
    for path in paths {
        let result = if recursive {
            fs::remove_dir_all(path).or_else(|_| fs::remove_file(path))
        } else {
            fs::remove_file(path)
        };

        if let Err(e) = result {
            eprintln!("rm: {}: {}", path, e);
            had_error = true;
        }
    }

    if had_error {
        process::exit(1);
    }
}
