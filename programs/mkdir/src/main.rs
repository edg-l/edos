//! mkdir - create directories

use std::env;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut parents = false;
    let mut paths: Vec<String> = Vec::new();

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "-p" => parents = true,
            _ if arg.starts_with('-') && arg.len() > 1 => {
                eprintln!("mkdir: unknown option '{}'", arg);
                return ExitCode::FAILURE;
            }
            _ => paths.push(arg),
        }
    }

    if paths.is_empty() {
        eprintln!("Usage: mkdir [-p] <path>...");
        return ExitCode::FAILURE;
    }

    let mut failed = false;
    for path in &paths {
        // With -p an existing directory is not an error, and every missing
        // component is created.
        let result = if parents {
            fs::create_dir_all(path)
        } else {
            fs::create_dir(path)
        };
        if let Err(e) = result {
            eprintln!("mkdir: {}: {}", path, e);
            failed = true;
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
