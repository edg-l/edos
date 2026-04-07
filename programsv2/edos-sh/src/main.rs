//! EDOS Shell - Command-line shell for EDOS GUI terminal.

mod builtins;
mod command;
mod complete;
mod script;
mod spawn;

use std::io::Write;

use edos_lib::io::{poll_stdin, sys_read};

pub enum SegmentResult {
    /// Command ran with the given exit code (0 = success, non-zero = failure).
    Done(i32),
    /// Shell should exit.
    Exit,
}

/// Run a single command segment (may be a pipeline or single command with redirects).
/// Returns a `SegmentResult` carrying the exit code.
pub fn run_segment(segment: &str) -> SegmentResult {
    let stages = command::split_pipeline(segment);
    if stages.is_empty() {
        return SegmentResult::Done(0);
    }

    if stages.len() > 1 {
        // Pipeline: parse each stage and spawn connected by pipes
        let parsed: Vec<(String, Vec<String>)> = stages
            .iter()
            .filter_map(|s| {
                let expanded = command::expand_variables(s);
                let args = command::parse_command(&expanded);
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
        // Pipeline exit code is not tracked; treat as success
        return SegmentResult::Done(0);
    }

    // Single command with possible redirects
    let expanded = command::expand_variables(&stages[0]);
    let args = command::parse_command(&expanded);
    if args.is_empty() {
        return SegmentResult::Done(0);
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
            return SegmentResult::Done(1);
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
            return SegmentResult::Done(1);
        }
        Some(fd as u64)
    } else {
        None
    };

    let exec_result = if stdout_fd.is_some() || stdin_fd.is_some() {
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
            edos_lib::io::pty_set_canonical(0);
            let result = if let Some(pid) =
                edos_lib::process::spawn_program_with_fds(cmd, &rest, in_fd, out_fd, 2)
            {
                let code = edos_lib::process::waitpid(pid);
                if code == 0 {
                    command::ExecResult::Success(0)
                } else {
                    command::ExecResult::Failed(code)
                }
            } else {
                eprintln!("Command not found: {}", cmd);
                command::ExecResult::Failed(127)
            };
            edos_lib::io::pty_set_raw(0);
            result
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

    match exec_result {
        command::ExecResult::Success(code) => SegmentResult::Done(code),
        command::ExecResult::Failed(code) => SegmentResult::Done(code),
        command::ExecResult::Exit => SegmentResult::Exit,
    }
}

/// Run a full command chain (handles `&&`, `||`, `;` operators).
///
/// Returns the exit code of the last command executed, or -1 if the shell
/// should exit (e.g. the `exit` builtin was called).
pub fn run_chain(input: &str) -> i32 {
    // Strip comments before processing
    let input = command::strip_comment(input).to_string();
    let trimmed_input = input.trim();
    if trimmed_input.is_empty() {
        return 0;
    }

    let chain = command::split_chain(&input);
    if chain.is_empty() {
        return 0;
    }

    let mut prev_op: Option<command::ChainOp> = None;
    let mut last_exit: i32 = 0;

    for (segment, next_op) in &chain {
        match prev_op {
            Some(command::ChainOp::And) if last_exit != 0 => {
                prev_op = *next_op;
                continue;
            }
            Some(command::ChainOp::Or) if last_exit == 0 => {
                prev_op = *next_op;
                continue;
            }
            _ => {}
        }

        match run_segment(segment) {
            SegmentResult::Done(code) => {
                last_exit = code;
                command::set_last_exit_code(code);
            }
            SegmentResult::Exit => {
                return -1;
            }
        }

        prev_op = *next_op;
    }

    last_exit
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
/// Redraw the line from the cursor position to the end, then reposition cursor.
fn redraw_from_cursor(line: &str, cursor: usize, prompt_len: usize) {
    // Save cursor, clear from cursor to end of line, print remaining chars, restore cursor
    let remaining = &line[cursor..];
    // Clear from cursor to end of line, print remaining, move cursor back
    print!("\x1B[K{}", remaining);
    // Move cursor back to correct position
    let chars_after = line.len() - cursor;
    if chars_after > 0 {
        print!("\x1B[{}D", chars_after);
    }
    let _ = std::io::stdout().flush();
}

fn read_line(history: &[String], prompt: &str) -> Option<String> {
    print!("{}", prompt);
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    let mut cursor: usize = 0; // byte position in line
    let mut buf = [0u8; 1];
    let mut history_index = history.len();
    let mut saved_line = String::new();
    let prompt_len = prompt.len(); // approximate (ANSI codes make this inaccurate but OK)

    loop {
        let poll_ready = poll_stdin(100);
        let n = sys_read(0, &mut buf);

        if n < 0 {
            // EINTR from Ctrl+C or error: discard partial input, re-prompt
            return None;
        }
        if n == 0 {
            continue;
        }

        let ch = buf[0];

        if ch == b'\n' || ch == b'\r' {
            print!("\n");
            let _ = std::io::stdout().flush();
            line.push('\n');
            return Some(line);
        }

        if ch == 0x08 || ch == 0x7F {
            if cursor > 0 {
                cursor -= 1;
                line.remove(cursor);
                // Move cursor back, then redraw from there
                print!("\x08");
                redraw_from_cursor(&line, cursor, prompt_len);
            }
            continue;
        }

        if ch == 0x1B {
            let mut seq = [0u8; 2];
            if poll_stdin(20) {
                let n = sys_read(0, &mut seq[..1]);
                if n == 1 && seq[0] == b'[' {
                    if poll_stdin(20) {
                        let n = sys_read(0, &mut seq[1..2]);
                        if n == 1 {
                            match seq[1] {
                                b'A' => {
                                    // Up arrow
                                    if history_index > 0 {
                                        if history_index == history.len() {
                                            saved_line = line.clone();
                                        }
                                        history_index -= 1;
                                        line = history[history_index].clone();
                                        cursor = line.len();
                                        redraw_line(prompt, &line);
                                    }
                                }
                                b'B' => {
                                    // Down arrow
                                    if history_index < history.len() {
                                        history_index += 1;
                                        if history_index == history.len() {
                                            line = saved_line.clone();
                                        } else {
                                            line = history[history_index].clone();
                                        }
                                        cursor = line.len();
                                        redraw_line(prompt, &line);
                                    }
                                }
                                b'C' => {
                                    // Right arrow
                                    if cursor < line.len() {
                                        cursor += 1;
                                        print!("\x1B[C");
                                        let _ = std::io::stdout().flush();
                                    }
                                }
                                b'D' => {
                                    // Left arrow
                                    if cursor > 0 {
                                        cursor -= 1;
                                        print!("\x1B[D");
                                        let _ = std::io::stdout().flush();
                                    }
                                }
                                b'H' => {
                                    // Home
                                    if cursor > 0 {
                                        print!("\x1B[{}D", cursor);
                                        cursor = 0;
                                        let _ = std::io::stdout().flush();
                                    }
                                }
                                b'F' => {
                                    // End
                                    if cursor < line.len() {
                                        print!("\x1B[{}C", line.len() - cursor);
                                        cursor = line.len();
                                        let _ = std::io::stdout().flush();
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            continue;
        }

        if ch == b'\t' {
            // Tab completion: find the current word under the cursor
            let line_before_cursor = &line[..cursor];
            // Find start of the current word (scan back to last space)
            let word_start = line_before_cursor
                .rfind(|c: char| c == ' ')
                .map(|p| p + 1)
                .unwrap_or(0);
            let word = &line[word_start..cursor];

            // Determine if this is command position (first token on line)
            let is_command_pos = line_before_cursor[..word_start].trim().is_empty();

            let candidates = if word.starts_with('$') {
                complete::complete_env_var(word)
            } else if is_command_pos && !word.contains('/') {
                complete::complete_command(word)
            } else {
                complete::complete_path(word)
            };

            if candidates.is_empty() {
                // No matches: ring bell
                print!("\x07");
                let _ = std::io::stdout().flush();
            } else if candidates.len() == 1 {
                // Single match: replace the current word with the completion
                let completion = &candidates[0];
                // Remove the current word from the line
                line.drain(word_start..cursor);
                line.insert_str(word_start, completion);
                // Add a trailing space if the completion is not a directory
                let add_space = !completion.ends_with('/');
                if add_space {
                    line.insert(word_start + completion.len(), ' ');
                }
                cursor = word_start + completion.len() + usize::from(add_space);
                redraw_line(prompt, &line);
                // Move cursor to end (redraw_line goes to end of line)
            } else {
                // Multiple matches: fill common prefix, print candidates
                let common = complete::longest_common_prefix(&candidates);
                if common.len() > word.len() {
                    // We can extend the current word to the common prefix
                    line.drain(word_start..cursor);
                    line.insert_str(word_start, &common);
                    cursor = word_start + common.len();
                }
                // Print candidates on a new line, then reprint prompt and line
                print!("\r\n");
                for (i, c) in candidates.iter().enumerate() {
                    if i > 0 {
                        print!("  ");
                    }
                    print!("{}", c);
                }
                print!("\r\n");
                redraw_line(prompt, &line);
                // redraw_line puts cursor at end; reposition to `cursor`
                let chars_after = line.len() - cursor;
                if chars_after > 0 {
                    print!("\x1B[{}D", chars_after);
                }
                let _ = std::io::stdout().flush();
            }
            continue;
        }

        if ch < 0x20 {
            continue;
        }

        // Insert character at cursor position
        if cursor == line.len() {
            // Append (common case)
            line.push(ch as char);
            cursor += 1;
            print!("{}", ch as char);
            let _ = std::io::stdout().flush();
        } else {
            // Insert in middle
            line.insert(cursor, ch as char);
            cursor += 1;
            // Print from insertion point to end, then move cursor back
            print!("{}", &line[cursor - 1..]);
            let chars_after = line.len() - cursor;
            if chars_after > 0 {
                print!("\x1B[{}D", chars_after);
            }
            let _ = std::io::stdout().flush();
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();

    // Set default environment variables if not already inherited.
    // SAFETY: single-threaded at this point; no other threads exist yet.
    unsafe {
        if std::env::var("PATH").is_err() {
            std::env::set_var("PATH", "/bin");
        }
        if std::env::var("HOME").is_err() {
            std::env::set_var("HOME", "/");
        }
        if std::env::var("PWD").is_err() {
            if let Ok(cwd) = std::env::current_dir() {
                std::env::set_var("PWD", cwd);
            }
        }
    }

    // Check invocation mode based on arguments
    // argv[0] is the shell itself
    let mut idx = 1usize;
    let mut exit_on_error = false;

    // Parse leading flags (-e, -c)
    while idx < argv.len() && argv[idx].starts_with('-') && argv[idx].len() > 1 {
        match argv[idx].as_str() {
            "-e" => {
                exit_on_error = true;
                idx += 1;
            }
            "-c" => {
                // -c "command" [args...]
                idx += 1;
                if idx >= argv.len() {
                    eprintln!("sh: -c: option requires an argument");
                    std::process::exit(1);
                }
                let cmd = argv[idx].clone();
                // Positional params: $0 = "sh", rest are additional args
                let mut params = vec!["sh".to_string()];
                params.extend_from_slice(&argv[idx + 1..]);
                command::set_script_args(&params);
                let code = run_chain(&cmd);
                std::process::exit(if code == -1 { 0 } else { code });
            }
            _ => {
                // Unknown flag: stop flag parsing and treat rest as script path
                break;
            }
        }
    }

    if exit_on_error {
        command::set_exit_on_error(true);
    }

    if idx < argv.len() {
        // Script mode: sh [script.sh] [args...]
        let script_path = argv[idx].clone();
        let mut script_params = vec![script_path.clone()];
        script_params.extend_from_slice(&argv[idx + 1..]);
        command::set_script_args(&script_params);
        let code = script::run_script(&script_path, &script_params);
        std::process::exit(code);
    }

    // Interactive mode
    command::set_script_args(&argv);

    edos_lib::io::pty_set_raw(0);

    println!("EDOS Shell v0.1");
    println!("Type 'help' for commands.\n");

    let mut stdout = std::io::stdout();

    // Flush welcome message immediately so terminal can display it
    let _ = stdout.flush();

    let mut history: Vec<String> = Vec::new();
    // Load history from file
    if let Ok(content) = std::fs::read_to_string("/tmp/.sh_history") {
        for line in content.lines() {
            if !line.is_empty() {
                history.push(line.to_string());
            }
        }
        // Cap at 1000 entries
        if history.len() > 1000 {
            history.drain(..history.len() - 1000);
        }
    }

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
            history.push(trimmed.clone());
            // Append to history file
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/.sh_history")
            {
                let _ = writeln!(f, "{}", trimmed);
            }
        }

        // Handle `history` builtin before splitting (needs access to history vec)
        if trimmed == "history" {
            builtins::cmd_history(&history);
            continue;
        }

        let result = run_chain(&input);
        if result == -1 {
            break;
        }
    }
}
