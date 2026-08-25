use std::env;

use edos_lib::process::{SIGTERM, kill, signal_by_name};

/// `kill [-SIGNAL] PID...`, the same form the shell builtin takes. Both exist
/// because the builtin shadows this binary only when the shell runs it as a
/// bare command word; a script that calls `/bin/kill` reaches this one, and
/// two argument orders for one command name is a way to terminate a process
/// you meant to suspend.
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let (signal, pids) = match args.split_first() {
        Some((first, rest)) if first.starts_with('-') => match signal_by_name(&first[1..]) {
            Some(sig) => (sig, rest),
            None => {
                eprintln!("kill: unknown signal: {}", &first[1..]);
                std::process::exit(1);
            }
        },
        Some(_) => (SIGTERM, args.as_slice()),
        None => {
            eprintln!("usage: kill [-SIGNAL] PID...");
            std::process::exit(1);
        }
    };

    if pids.is_empty() {
        eprintln!("usage: kill [-SIGNAL] PID...");
        std::process::exit(1);
    }

    let mut status = 0;
    for pid_arg in pids {
        let Ok(pid) = pid_arg.parse::<u64>() else {
            eprintln!("kill: invalid pid: {}", pid_arg);
            status = 1;
            continue;
        };
        if kill(pid, signal).is_err() {
            eprintln!("kill: failed to send signal {} to pid {}", signal, pid);
            status = 1;
        }
    }
    std::process::exit(status);
}
