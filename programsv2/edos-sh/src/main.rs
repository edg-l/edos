//! EDOS Shell - Command-line shell for EDOS GUI terminal.

mod builtins;
mod command;
mod spawn;

use std::io::Write;

use edos_lib::io::{poll_stdin, sys_read};

enum SegmentResult {
    Ok,
    Failed,
    Exit,
}

/// Run a single command segment (may be a pipeline or single command with redirects).
fn run_segment(segment: &str) -> SegmentResult {
    let stages = command::split_pipeline(segment);
    if stages.is_empty() {
        return SegmentResult::Ok;
    }

    if stages.len() > 1 {
        // Pipeline: parse each stage and spawn connected by pipes
        let parsed: Vec<(String, Vec<String>)> = stages
            .iter()
            .filter_map(|s| {
                let args = command::parse_command(s);
                if args.is_empty() {
                    None
                } else {
                    let (cmd, rest) = args.split_at(1);
                    Some((cmd[0].clone(), rest.to_vec()))
                }
            })
            .collect();
        if !parsed.is_empty() {
            spawn::spawn_pipeline(&parsed);
        }
        return SegmentResult::Ok;
    }

    // Single command with possible redirects
    let args = command::parse_command(&stages[0]);
    if args.is_empty() {
        return SegmentResult::Ok;
    }
    let (cmd, rest) = args.split_first().unwrap();
    let (rest, redirects) = command::extract_redirects(rest);

    // Open redirect files
    let stdout_fd = if let Some(ref path) = redirects.stdout_file {
        let flags: u64 = if redirects.stdout_append {
            0x40 | 0x400
        } else {
            0x40
        };
        let fd = edos_lib::io::open(path, flags);
        if fd < 0 {
            eprintln!("{}: cannot open for writing", path);
            return SegmentResult::Failed;
        }
        Some(fd as u64)
    } else {
        None
    };

    let stdin_fd = if let Some(ref path) = redirects.stdin_file {
        let fd = edos_lib::io::open(path, 0);
        if fd < 0 {
            eprintln!("{}: cannot open for reading", path);
            if let Some(fd) = stdout_fd {
                edos_lib::process::close(fd);
            }
            return SegmentResult::Failed;
        }
        Some(fd as u64)
    } else {
        None
    };

    let result = if stdout_fd.is_some() || stdin_fd.is_some() {
        if command::is_builtin(cmd) {
            let saved_stdout = stdout_fd.map(|fd| {
                let saved = edos_lib::process::dup(1) as u64;
                edos_lib::process::dup2(fd, 1);
                saved
            });
            let saved_stdin = stdin_fd.map(|fd| {
                let saved = edos_lib::process::dup(0) as u64;
                edos_lib::process::dup2(fd, 0);
                saved
            });

            let r = command::execute_command(cmd, &rest);

            let _ = std::io::Write::flush(&mut std::io::stdout());

            if let Some(saved) = saved_stdout {
                edos_lib::process::dup2(saved, 1);
                edos_lib::process::close(saved);
            }
            if let Some(saved) = saved_stdin {
                edos_lib::process::dup2(saved, 0);
                edos_lib::process::close(saved);
            }
            r
        } else {
            let in_fd = stdin_fd.unwrap_or(0);
            let out_fd = stdout_fd.unwrap_or(1);
            if let Some(pid) =
                edos_lib::process::spawn_program_with_fds(cmd, &rest, in_fd, out_fd, 2)
            {
                edos_lib::process::waitpid(pid);
                command::ExecResult::Ok
            } else {
                eprintln!("Command not found: {}", cmd);
                command::ExecResult::NotFound
            }
        }
    } else {
        command::execute_command(cmd, &rest)
    };

    // Close redirect file fds
    if let Some(fd) = stdout_fd {
        edos_lib::process::close(fd);
    }
    if let Some(fd) = stdin_fd {
        edos_lib::process::close(fd);
    }

    match result {
        command::ExecResult::Ok => SegmentResult::Ok,
        command::ExecResult::NotFound => SegmentResult::Failed,
        command::ExecResult::Exit => SegmentResult::Exit,
    }
}

/// Redraw the current input line after history navigation.
fn redraw_line(prompt: &str, line: &str) {
    print!("\r\x1B[2K{}{}", prompt, line);
    let _ = std::io::stdout().flush();
}

/// Read a line from stdin, using poll() to efficiently wait for input.
/// Returns the line (including newline), or None on EOF/error.
///
/// Uses a hybrid approach: poll for efficiency, but always attempt read
/// regardless of poll result to handle race conditions where poll might
/// miss events.
///
/// Up/Down arrow keys navigate command history.
fn read_line(history: &[String], prompt: &str) -> Option<String> {
    print!("{}", prompt);
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    let mut buf = [0u8; 1];
    let mut history_index = history.len(); // start past end = "new line"
    let mut saved_line = String::new(); // saves current input when browsing history

    loop {
        // Try poll first with short timeout for efficiency
        let poll_ready = poll_stdin(100);

        // Always try to read (non-blocking) regardless of poll result.
        // This provides resilience against poll edge cases/races.
        let n = sys_read(0, &mut buf);

        if n < 0 {
            // Error
            return if line.is_empty() { None } else { Some(line) };
        }

        if n == 0 {
            // No data available
            if !poll_ready {
                // Poll timed out and no data - continue polling
                continue;
            }
            // Poll said readable but no data - spurious, retry
            continue;
        }

        let ch = buf[0];

        // Treat both '\r' (carriage return) and '\n' (newline) as line terminators.
        // Terminal sends '\r' for Enter key; normalize to '\n'.
        if ch == b'\n' || ch == b'\r' {
            print!("\n");
            let _ = std::io::stdout().flush();
            line.push('\n');
            return Some(line);
        }

        // Handle backspace: remove last character if any
        if ch == 0x08 || ch == 0x7F {
            if !line.is_empty() {
                line.pop();
                // Send backspace sequence to terminal: move back, overwrite with space, move back
                print!("\x08 \x08");
                let _ = std::io::stdout().flush();
            }
            continue;
        }

        if ch == 0x1B {
            // Try to read escape sequence
            let mut seq = [0u8; 2];
            // Short poll + read for the bracket
            if poll_stdin(20) {
                let n = sys_read(0, &mut seq[..1]);
                if n == 1 && seq[0] == b'[' {
                    // Read the final byte
                    if poll_stdin(20) {
                        let n = sys_read(0, &mut seq[1..2]);
                        if n == 1 {
                            match seq[1] {
                                b'A' => {
                                    // Up arrow: go back in history
                                    if history_index > 0 {
                                        if history_index == history.len() {
                                            saved_line = line.clone();
                                        }
                                        history_index -= 1;
                                        line = history[history_index].clone();
                                        redraw_line(prompt, &line);
                                    }
                                }
                                b'B' => {
                                    // Down arrow: go forward in history
                                    if history_index < history.len() {
                                        history_index += 1;
                                        if history_index == history.len() {
                                            line = saved_line.clone();
                                        } else {
                                            line = history[history_index].clone();
                                        }
                                        redraw_line(prompt, &line);
                                    }
                                }
                                _ => {} // ignore other sequences
                            }
                        }
                    }
                }
            }
            continue;
        }

        // Skip other control characters
        if ch < 0x20 && ch != b'\t' {
            continue;
        }

        line.push(ch as char);
        // Echo the character
        print!("{}", ch as char);
        let _ = std::io::stdout().flush();
    }
}

fn main() {
    println!("EDOS Shell v0.1");
    println!("Type 'help' for commands.\n");

    let mut stdout = std::io::stdout();

    // Flush welcome message immediately so terminal can display it
    let _ = stdout.flush();

    let mut history: Vec<String> = Vec::new();

    loop {
        // Build prompt with current directory
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".to_string());
        let prompt = format!("\x1B[32m{}\x1B[0m \x1B[1;34m$\x1B[0m ", cwd);

        // Flush stdout (important for piped I/O)
        if stdout.flush().is_err() {
            break;
        }

        // Read line from stdin
        let Some(input) = read_line(&history, &prompt) else {
            break; // EOF
        };

        // Push non-empty, non-duplicate commands to history
        let trimmed = input.trim().to_string();
        if !trimmed.is_empty() && history.last().map(|h| h != &trimmed).unwrap_or(true) {
            history.push(trimmed);
        }

        // Split on `&&`, `||`, `;`
        let chain = command::split_chain(&input);
        if chain.is_empty() {
            continue;
        }

        let mut prev_op: Option<command::ChainOp> = None;
        let mut last_ok = true;
        let mut should_exit = false;

        for (segment, next_op) in &chain {
            // Skip based on previous chain operator
            match prev_op {
                Some(command::ChainOp::And) if !last_ok => {
                    prev_op = *next_op;
                    continue;
                }
                Some(command::ChainOp::Or) if last_ok => {
                    prev_op = *next_op;
                    continue;
                }
                _ => {}
            }

            match run_segment(segment) {
                SegmentResult::Ok => last_ok = true,
                SegmentResult::Failed => last_ok = false,
                SegmentResult::Exit => {
                    should_exit = true;
                    break;
                }
            }

            prev_op = *next_op;
        }

        if should_exit {
            break;
        }
    }
}
