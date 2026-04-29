use std::env;

const SIGTERM: u32 = 15;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: kill <pid> [signal]");
        std::process::exit(1);
    }

    let pid: u64 = match args[1].parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("kill: invalid pid: {}", args[1]);
            std::process::exit(1);
        }
    };

    let signal = if args.len() > 2 {
        let s = args[2].trim();
        if let Some(stripped) = s.strip_prefix('-') {
            match stripped.parse::<u32>() {
                Ok(n) if n > 0 && n < 32 => n,
                _ => {
                    eprintln!("kill: invalid signal: {}", args[2]);
                    std::process::exit(1);
                }
            }
        } else {
            match s.parse::<u32>() {
                Ok(n) if n > 0 && n < 32 => n,
                _ => {
                    eprintln!("kill: invalid signal: {}", args[2]);
                    std::process::exit(1);
                }
            }
        }
    } else {
        SIGTERM
    };

    let ret = edos_lib::process::sys_kill(pid, signal);
    if ret != 0 {
        eprintln!("kill: failed to send signal {} to pid {}", signal, pid);
        std::process::exit(1);
    }
}
