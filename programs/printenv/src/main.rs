//! Print environment variables.
//!
//! With no operand this is `env` with nothing to run: one `NAME=value` line per
//! variable. With operands it prints only the named variables' values, one per
//! line, and exits 1 if any of them is unset -- which is what makes it useful
//! in a script, where `printenv HOME` succeeds or fails rather than always
//! printing something.

use std::{env, process::ExitCode};

const USAGE: &str = "usage: printenv [-0] [name...]";

fn main() -> ExitCode {
    let mut names: Vec<String> = Vec::new();
    let mut nul = false;
    let mut operands = false;

    for arg in env::args().skip(1) {
        // Only options before the first operand are options: `printenv -0` asks
        // for NUL separators, `printenv PATH -0` asks for a variable called -0.
        if !operands && arg.starts_with('-') && arg.len() > 1 {
            match arg.as_str() {
                "-0" | "--null" => nul = true,
                "-h" | "--help" => {
                    println!("{USAGE}");
                    return ExitCode::SUCCESS;
                }
                other => {
                    eprintln!("printenv: unknown option '{other}'\n{USAGE}");
                    return ExitCode::from(2);
                }
            }
            continue;
        }
        operands = true;
        names.push(arg);
    }

    let sep = if nul { '\0' } else { '\n' };

    if names.is_empty() {
        for (key, value) in env::vars() {
            print!("{key}={value}{sep}");
        }
        return ExitCode::SUCCESS;
    }

    let mut missing = false;
    for name in names {
        match env::var(&name) {
            Ok(value) => print!("{value}{sep}"),
            Err(_) => missing = true,
        }
    }
    if missing {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
