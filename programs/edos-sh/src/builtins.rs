//! Built-in shell commands (only commands that need shell process state).

use std::io::Write;

/// Print help message with available commands.
pub fn cmd_help() {
    println!("Builtins:");
    println!("  help              - Show this help");
    println!("  pwd               - Print working directory");
    println!("  cd [path]         - Change directory");
    println!("  clear             - Clear screen");
    println!("  echo [args...]    - Print arguments");
    println!("  exit              - Exit shell");
    println!("  kill [-SIGNAL] PID - Send signal to process");
    println!("  jobs              - List background jobs");
    println!("  wait [PID]        - Wait for background job(s)");
    println!("  ifconfig          - Show network configuration");
    println!();
    println!("Operators:");
    println!("  $((expr))         - Arithmetic expansion");
    println!("  cmd1 | cmd2       - Pipe output");
    println!("  cmd > file        - Redirect stdout (truncate)");
    println!("  cmd >> file       - Redirect stdout (append)");
    println!("  cmd < file        - Redirect stdin");
    println!("  cmd <<MARKER      - Heredoc (input until MARKER)");
    println!("  cmd <<'MARKER'    - Heredoc (no variable expansion)");
    println!("  cmd1 && cmd2      - Run cmd2 if cmd1 succeeds");
    println!("  cmd1 || cmd2      - Run cmd2 if cmd1 fails");
    println!("  cmd1 ; cmd2       - Run both unconditionally");
    println!("  cmd &             - Run cmd in background");
    println!("  (cmd1; cmd2)      - Run in subshell");
    println!();
    println!("Script control:");
    println!("  function name/end - Define a function");
    println!("  return [N]        - Return from function");
    println!();
    println!("External commands in /bin/:");
    println!("  ls cat stat free ps dmesg mkdir rmdir rm mv write echo");
}

/// Print current working directory.
pub fn cmd_pwd() {
    match std::env::current_dir() {
        Ok(path) => println!("{}", path.display()),
        Err(e) => eprintln!("pwd: error getting current directory: {}", e),
    }
}

/// Change current directory.
pub fn cmd_cd(args: &[String]) {
    let target = match args.first() {
        Some(path) => path.clone(),
        None => std::env::var("HOME").unwrap_or_else(|_| "/".to_string()),
    };

    match std::env::set_current_dir(&target) {
        Ok(()) => {
            // Keep PWD in sync so child processes inherit the correct directory
            match std::env::current_dir() {
                Ok(abs) => unsafe { std::env::set_var("PWD", abs) },
                Err(_) => unsafe { std::env::set_var("PWD", &target) },
            }
        }
        Err(e) => eprintln!("cd: {}: {}", target, e),
    }
}

/// Clear the terminal screen.
pub fn cmd_clear() {
    print!("\x1B[2J\x1B[H");
    let _ = std::io::stdout().flush();
}

/// Echo arguments to stdout.
pub fn cmd_echo(args: &[String]) {
    // Support -e flag for escape sequences
    if args.first().map(|s| s.as_str()) == Some("-e") {
        let text = args[1..].join(" ");
        print!("{}\n", expand_escapes(&text));
    } else {
        println!("{}", args.join(" "));
    }
}

/// Set or display environment variables.
///
/// With no args: print all env vars.
/// With `NAME=VALUE`: set the variable.
/// With `NAME` (no `=`): print that variable's value.
pub fn cmd_export(args: &[String]) {
    if args.is_empty() {
        for (key, val) in std::env::vars() {
            println!("{}={}", key, val);
        }
        return;
    }
    for arg in args {
        if let Some(eq_pos) = arg.find('=') {
            let key = &arg[..eq_pos];
            let val = &arg[eq_pos + 1..];
            unsafe { std::env::set_var(key, val) };
        } else {
            // No `=`: print current value of that variable
            match std::env::var(arg) {
                Ok(val) => println!("{}={}", arg, val),
                Err(_) => eprintln!("export: {}: not set", arg),
            }
        }
    }
}

/// Remove environment variables.
pub fn cmd_unset(args: &[String]) {
    for arg in args {
        unsafe { std::env::remove_var(arg) };
    }
}

/// Print command history with numbered entries.
pub fn cmd_history(history: &[String]) {
    for (i, cmd) in history.iter().enumerate() {
        println!("  {}  {}", i + 1, cmd);
    }
}

/// Print all environment variables in KEY=VALUE format.
pub fn cmd_env() {
    for (key, val) in std::env::vars() {
        println!("{}={}", key, val);
    }
}

/// Evaluate the `test` / `[` builtin expression.
///
/// Returns 0 for true, 1 for false, 2 for a usage error.
pub fn cmd_test(args: &[String]) -> i32 {
    match args {
        [] => 1, // no expression → false
        [flag, operand] if flag == "!" => {
            // Negate a unary expression: `! EXPR` where EXPR is a single word
            // Treat a non-empty string as true (mirrors `test STRING`)
            let inner = cmd_test(&[operand.clone()]);
            if inner == 2 {
                2
            } else if inner == 0 {
                1
            } else {
                0
            }
        }
        [flag, operand] => match flag.as_str() {
            "-f" => {
                if std::fs::metadata(operand)
                    .map(|m| m.is_file())
                    .unwrap_or(false)
                {
                    0
                } else {
                    1
                }
            }
            "-d" => {
                if std::fs::metadata(operand)
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
                {
                    0
                } else {
                    1
                }
            }
            "-e" => {
                if std::fs::metadata(operand).is_ok() {
                    0
                } else {
                    1
                }
            }
            "-z" => {
                if operand.is_empty() {
                    0
                } else {
                    1
                }
            }
            "-n" => {
                if !operand.is_empty() {
                    0
                } else {
                    1
                }
            }
            _ => {
                // Single argument (no flag): true if non-empty string
                // The flag here is actually the only argument; operand is a second word.
                // This arm won't match single-arg invocations; fall through to the
                // three-argument arm below.
                2
            }
        },
        [lhs, op, rhs] => match op.as_str() {
            "=" => {
                if lhs == rhs {
                    0
                } else {
                    1
                }
            }
            "!=" => {
                if lhs != rhs {
                    0
                } else {
                    1
                }
            }
            "-eq" | "-ne" | "-lt" | "-gt" | "-le" | "-ge" => {
                let a: i64 = match lhs.parse() {
                    Ok(v) => v,
                    Err(_) => {
                        eprintln!("test: {}: integer expression expected", lhs);
                        return 2;
                    }
                };
                let b: i64 = match rhs.parse() {
                    Ok(v) => v,
                    Err(_) => {
                        eprintln!("test: {}: integer expression expected", rhs);
                        return 2;
                    }
                };
                let result = match op.as_str() {
                    "-eq" => a == b,
                    "-ne" => a != b,
                    "-lt" => a < b,
                    "-gt" => a > b,
                    "-le" => a <= b,
                    "-ge" => a >= b,
                    _ => unreachable!(),
                };
                if result { 0 } else { 1 }
            }
            _ => {
                eprintln!("test: unknown operator: {}", op);
                2
            }
        },
        [flag, operand, ..] if flag == "!" => {
            // `! unary_flag operand` — negate a unary test
            let inner = cmd_test(&args[1..]);
            if inner == 2 {
                2
            } else if inner == 0 {
                1
            } else {
                0
            }
        }
        [single] => {
            // Single non-flag argument: true if non-empty
            if !single.is_empty() { 0 } else { 1 }
        }
        _ => {
            eprintln!("test: too many arguments");
            2
        }
    }
}

/// Kill a process: kill [-SIGNAL] PID
pub fn cmd_kill(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("usage: kill [-SIGNAL] PID");
        return 1;
    }

    let (signal, pid_arg) = if args[0].starts_with('-') {
        let sig_str = &args[0][1..];
        let sig = match sig_str {
            "INT" | "2" => 2u32,
            "KILL" | "9" => 9,
            "TERM" | "15" => 15,
            "HUP" | "1" => 1,
            other => {
                if let Ok(n) = other.parse::<u32>() {
                    n
                } else {
                    eprintln!("kill: unknown signal: {}", other);
                    return 1;
                }
            }
        };
        if args.len() < 2 {
            eprintln!("usage: kill [-SIGNAL] PID");
            return 1;
        }
        (sig, &args[1])
    } else {
        (15u32, &args[0]) // Default: SIGTERM
    };

    let pid: u64 = match pid_arg.parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("kill: invalid PID: {}", pid_arg);
            return 1;
        }
    };

    let result = edos_lib::process::sys_kill(pid, signal);
    if result < 0 {
        eprintln!("kill: no such process: {}", pid);
        1
    } else {
        0
    }
}

/// Print network interface configuration.
pub fn cmd_ifconfig(_args: &[String]) {
    let mut buf = [0u8; 512];
    let len = unsafe {
        edos_lib::sys::syscall2(
            edos_lib::sys::SYS_NETINFO,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
    if len != u64::MAX {
        let s = std::str::from_utf8(&buf[..len as usize]).unwrap_or("invalid");
        print!("{}", s);
    } else {
        eprintln!("Failed to get network info");
    }
}

/// Expand backslash escape sequences in a string.
fn expand_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('e') => out.push('\x1B'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}
