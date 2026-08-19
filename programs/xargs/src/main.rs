use std::env;
use std::io::{self, BufRead};

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut cmd = String::new();
    let mut cmd_args: Vec<String> = Vec::new();
    // Collect command and args; special case "-0" for NUL-delimited (ignored for now)
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if a == "-0" {
            i += 1;
            continue;
        }
        if cmd.is_empty() {
            cmd = a.clone();
        } else {
            cmd_args.push(a.clone());
        }
        i += 1;
    }

    if cmd.is_empty() {
        eprintln!("usage: xargs <command> [args...]");
        std::process::exit(1);
    }

    // Read lines from stdin
    let stdin = io::stdin();
    let reader = stdin.lock();
    let mut collected: Vec<String> = Vec::new();
    let mut exit_code: i32 = 0;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        collected.push(trimmed.to_string());

        // Execute in batches of 256 to avoid huge arg lists
        if collected.len() >= 256 {
            if run_command(&cmd, &cmd_args, &collected) != 0 {
                exit_code = 1;
            }
            collected.clear();
        }
    }

    if !collected.is_empty()
        && run_command(&cmd, &cmd_args, &collected) != 0 {
            exit_code = 1;
        }

    std::process::exit(exit_code);
}

fn run_command(cmd: &str, cmd_args: &[String], extra_args: &[String]) -> i32 {
    let mut all_args: Vec<String> = cmd_args.to_vec();
    for a in extra_args {
        all_args.push(a.clone());
    }

    let pid = edos_lib::process::spawn_program_with_fds(cmd, &all_args, 0, 1, 2);
    match pid {
        Some(pid) => {
            let status = edos_lib::process::waitpid(pid);
            if status != 0 { 1 } else { 0 }
        }
        None => {
            eprintln!("xargs: command not found: {}", cmd);
            1
        }
    }
}
