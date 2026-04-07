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
    println!();
    println!("Operators:");
    println!("  cmd1 | cmd2       - Pipe output");
    println!("  cmd > file        - Redirect stdout (truncate)");
    println!("  cmd >> file       - Redirect stdout (append)");
    println!("  cmd < file        - Redirect stdin");
    println!("  cmd1 && cmd2      - Run cmd2 if cmd1 succeeds");
    println!("  cmd1 || cmd2      - Run cmd2 if cmd1 fails");
    println!("  cmd1 ; cmd2       - Run both unconditionally");
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
    let target = args.first().map(String::as_str).unwrap_or("/");

    if let Err(e) = std::env::set_current_dir(target) {
        eprintln!("cd: {}: {}", target, e);
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
