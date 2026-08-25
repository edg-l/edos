//! mkfifo - create named pipes

use std::env;
use std::process::ExitCode;

use edos_lib::io;

fn main() -> ExitCode {
    let paths: Vec<String> = env::args().skip(1).collect();

    if paths.is_empty() || paths.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("Usage: mkfifo <path>...");
        eprintln!();
        eprintln!("Create a named pipe: a channel two programs with no common");
        eprintln!("parent can meet on. Opening one end waits for the other.");
        return ExitCode::FAILURE;
    }

    let mut failed = false;
    for path in &paths {
        if io::mkfifo(path).is_err() {
            eprintln!("mkfifo: {}: {:?}", path, io::last_errno());
            failed = true;
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
