//! EDOS Shell - Command-line shell for EDOS GUI terminal.

mod arith;
mod builtins;
mod command;
mod complete;
mod glob;
mod jobs;
mod script;
mod spawn;

use std::io::Write;
use std::sync::Mutex;

use edos_lib::config;
use edos_lib::io::{poll_stdin, sys_read};

/// Global background job list.
static JOB_LIST: Mutex<jobs::JobList> = Mutex::new(jobs::JobList::new());

/// The shell's own process group, captured at startup. The terminal goes back
/// to it every time a foreground job ends.
static SHELL_PGID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether this shell runs jobs in process groups of their own.
///
/// Only an interactive shell does. `sh -c` and a script leave every command in
/// the group they were started in, so one signal aimed at that group reaches
/// the whole tree: `sshd` hanging up on a session kills the command it ran and
/// not just the shell that ran it.
static JOB_CONTROL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn job_control() -> bool {
    JOB_CONTROL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Put a freshly spawned pipeline in a group of its own, led by its first
/// stage, so that one Ctrl+C reaches every stage and no other job.
fn group_job(pids: &[u64]) {
    if !job_control() {
        return;
    }
    let Some(&leader) = pids.first() else {
        return;
    };
    for &pid in pids {
        let _ = edos_lib::process::setpgid(pid, leader);
    }
}

/// Take over job control: lead a group of the shell's own, hold the terminal,
/// and record that group so it can be handed back after every job.
///
/// A child inherits its spawner's group, so a shell that did not do this would
/// share the terminal's foreground group with everything it runs, and Ctrl+C
/// would reach the shell along with the job.
fn claim_terminal() {
    JOB_CONTROL.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = edos_lib::process::setpgid(0, 0);
    if let Ok(pgid) = edos_lib::process::getpgid(0)
        && pgid > 0
    {
        SHELL_PGID.store(pgid, std::sync::atomic::Ordering::Relaxed);
    }
    reclaim_terminal();
}

/// Take the terminal back from a job.
///
/// Needed after every job, background ones included: otherwise the next Ctrl+C
/// goes to a job the user is not looking at.
fn reclaim_terminal() {
    let pgid = SHELL_PGID.load(std::sync::atomic::Ordering::Relaxed);
    if pgid != 0 {
        let _ = edos_lib::process::tcsetpgrp(0, pgid);
    }
}

pub enum SegmentResult {
    /// Command ran with the given exit code (0 = success, non-zero = failure).
    Done(i32),
    /// Shell should exit with the given code.
    Exit(i32),
}

/// A segment parsed, expanded and with its redirections opened, ready to run
/// either in the foreground or as a job.
enum Prepared {
    /// Nothing to run.
    Nothing,
    /// A redirection could not be opened; the message is already printed.
    Failed,
    /// A builtin, shell function or assignment: it runs in the shell itself.
    Builtin {
        cmd: String,
        args: Vec<String>,
        open: command::OpenRedirects,
    },
    /// One or more external programs, connected by pipes.
    External {
        stages: Vec<edos_lib::process::PipelineStage>,
        opened: Vec<command::OpenRedirects>,
    },
}

/// True for a command word the shell runs itself rather than spawning.
///
/// A bare `VAR=value` is included: `execute_command` recognises it, and falls
/// through to a spawn when the name is not a valid identifier.
fn runs_in_shell(cmd: &str) -> bool {
    command::is_builtin(cmd)
        || script::is_function(cmd)
        || (cmd.contains('=') && !cmd.starts_with('='))
}

/// Expand a segment and open its redirections exactly once.
///
/// Expansion has side effects — `$(cmd)` runs a command — so a segment is
/// prepared once and the result is what runs, in the foreground or as a job.
fn prepare_segment(segment: &str) -> Prepared {
    let stages = command::split_pipeline(segment);
    if stages.is_empty() {
        return Prepared::Nothing;
    }

    if stages.len() == 1 {
        let expanded = command::expand_variables(&stages[0]);
        let args = command::parse_command(&expanded);
        let Some((first, rest)) = args.split_first() else {
            return Prepared::Nothing;
        };
        let cmd = first.0.clone();
        let (rest, redirects) = command::extract_redirects(rest);
        let rest = glob::expand_words(&rest);
        let Some(open) = command::open_redirects(&redirects) else {
            return Prepared::Failed;
        };
        if runs_in_shell(&cmd) {
            return Prepared::Builtin {
                cmd,
                args: rest,
                open,
            };
        }
        return Prepared::External {
            stages: vec![edos_lib::process::PipelineStage {
                command: cmd,
                args: rest,
                slots: open.slots,
            }],
            opened: vec![open],
        };
    }

    // Pipeline: parse each stage and apply its own redirections on top of the
    // pipe wiring.
    let mut parsed: Vec<edos_lib::process::PipelineStage> = Vec::new();
    let mut opened: Vec<command::OpenRedirects> = Vec::new();
    for stage in &stages {
        let expanded = command::expand_variables(stage);
        let args = command::parse_command(&expanded);
        let Some((first, rest)) = args.split_first() else {
            continue;
        };
        let (rest, redirects) = command::extract_redirects(rest);
        let Some(open) = command::open_redirects(&redirects) else {
            for open in opened {
                open.close();
            }
            return Prepared::Failed;
        };
        parsed.push(edos_lib::process::PipelineStage {
            command: first.0.clone(),
            args: glob::expand_words(&rest),
            slots: open.slots,
        });
        opened.push(open);
    }
    if parsed.is_empty() {
        for open in opened {
            open.close();
        }
        return Prepared::Nothing;
    }
    Prepared::External {
        stages: parsed,
        opened,
    }
}

/// Run a single command segment (may be a pipeline or single command with redirects).
/// Returns a `SegmentResult` carrying the exit code.
pub fn run_segment(segment: &str) -> SegmentResult {
    match prepare_segment(segment) {
        Prepared::Nothing => SegmentResult::Done(0),
        Prepared::Failed => SegmentResult::Done(1),
        Prepared::Builtin { cmd, args, open } => {
            let exec_result = if open.is_default() {
                command::execute_command(&cmd, &args)
            } else {
                let fds = resolve_slots(&open.slots, [0, 1, 2]);
                run_builtin_redirected(&cmd, &args, fds)
            };
            open.close();
            match exec_result {
                command::ExecResult::Success(code) => SegmentResult::Done(code),
                command::ExecResult::Failed(code) => SegmentResult::Done(code),
                command::ExecResult::Exit(code) => SegmentResult::Exit(code),
            }
        }
        Prepared::External { stages, opened } => {
            SegmentResult::Done(run_foreground(&stages, opened, segment))
        }
    }
}

/// Exit code reported for a job the user suspended: the shell's convention of
/// 128 plus the signal number, SIGTSTP being 20.
const EXIT_STOPPED: i32 = 148;

/// Spawn a pipeline in the foreground: hand it the terminal, wait for it, and
/// take the terminal back. A stop turns it into a job.
fn run_foreground(
    stages: &[edos_lib::process::PipelineStage],
    opened: Vec<command::OpenRedirects>,
    segment: &str,
) -> i32 {
    // Restore canonical mode for the child (echo + line buffering).
    edos_lib::io::pty_set_canonical(0);
    let pids = spawn::spawn_pipeline(stages);
    group_job(&pids);
    for open in opened {
        open.close();
    }
    if pids.is_empty() {
        edos_lib::io::pty_set_raw(0);
        return 127;
    }

    if job_control() {
        let _ = edos_lib::process::tcsetpgrp(0, pids[0]);
    }
    let outcome = jobs::wait_foreground(&pids);
    reclaim_terminal();
    edos_lib::io::pty_set_raw(0);

    match outcome {
        jobs::Foreground::Exited(code) => code,
        jobs::Foreground::Stopped => {
            let command = segment.trim().to_string();
            let id = JOB_LIST
                .lock()
                .unwrap()
                .add(pids, command.clone(), jobs::JobStatus::Stopped);
            println!("\n[{}]+ Stopped    {}", id, command);
            EXIT_STOPPED
        }
    }
}

/// Start a segment as a background job and report it.
///
/// External commands are spawned directly, so the job *is* the pipeline and
/// `fg` can hand it the terminal. Anything the shell runs itself is forked
/// first, since a builtin has no process of its own.
fn run_background(segment: &str) -> i32 {
    let command = segment.trim().to_string();
    let pids = match prepare_segment(segment) {
        Prepared::Nothing => return 0,
        Prepared::Failed => return 1,
        Prepared::Builtin { cmd, args, open } => {
            let pid = edos_lib::process::fork();
            if pid == Ok(0) {
                // Under job control, lead a group of its own: a background job
                // must not be in the shell's group, where Ctrl+C would reach
                // it. A non-interactive shell has no job control and leaves it
                // where it is, so a hangup aimed at the session reaches it.
                if job_control() {
                    let _ = edos_lib::process::setpgid(0, 0);
                }
                let fds = resolve_slots(&open.slots, [0, 1, 2]);
                let result = if open.is_default() {
                    command::execute_command(&cmd, &args)
                } else {
                    run_builtin_redirected(&cmd, &args, fds)
                };
                let code = match result {
                    command::ExecResult::Success(code) => code,
                    command::ExecResult::Failed(code) => code,
                    command::ExecResult::Exit(code) => code,
                };
                std::process::exit(code);
            }
            open.close();
            let Ok(pid) = pid else {
                eprintln!("{}: fork failed", command);
                return 1;
            };
            vec![pid]
        }
        Prepared::External { stages, opened } => {
            let pids = spawn::spawn_pipeline(&stages);
            group_job(&pids);
            for open in opened {
                open.close();
            }
            // A background job must not hold the terminal.
            reclaim_terminal();
            if pids.is_empty() {
                return 127;
            }
            pids
        }
    };

    let leader = pids[0];
    let job_id = JOB_LIST
        .lock()
        .unwrap()
        .add(pids, command, jobs::JobStatus::Running);
    command::set_last_bg_pid(leader);
    println!("[{}] {}", job_id, leader);
    0
}

/// Resume a stopped job in the foreground, or bring a running one back.
pub fn cmd_fg(args: &[String]) -> i32 {
    let Some(id) = job_argument(args, "fg") else {
        return 1;
    };
    let Some(job) = JOB_LIST.lock().unwrap().take(id) else {
        eprintln!("fg: no such job: {}", id);
        return 1;
    };

    println!("{}", job.command);
    edos_lib::io::pty_set_canonical(0);
    let _ = edos_lib::process::tcsetpgrp(0, job.pgid);
    let _ = edos_lib::process::kill_group(job.pgid, edos_lib::process::SIGCONT);
    let outcome = jobs::wait_foreground(&job.pids);
    reclaim_terminal();
    edos_lib::io::pty_set_raw(0);

    match outcome {
        jobs::Foreground::Exited(code) => code,
        jobs::Foreground::Stopped => {
            println!("\n[{}]+ Stopped    {}", job.id, job.command);
            JOB_LIST
                .lock()
                .unwrap()
                .put_back(job, jobs::JobStatus::Stopped);
            EXIT_STOPPED
        }
    }
}

/// Resume a stopped job in the background.
pub fn cmd_bg(args: &[String]) -> i32 {
    let Some(id) = job_argument(args, "bg") else {
        return 1;
    };
    let Some(job) = JOB_LIST.lock().unwrap().resume(id) else {
        eprintln!("bg: no such job: {}", id);
        return 1;
    };
    let _ = edos_lib::process::kill_group(job.pgid, edos_lib::process::SIGCONT);
    println!("[{}]+ {} &", job.id, job.command);
    0
}

/// Read the `%n` or `n` job argument `fg`/`bg` takes, defaulting to the current
/// job. Prints the shell's message and returns None when there is none.
fn job_argument(args: &[String], builtin: &str) -> Option<u32> {
    match args.first() {
        Some(spec) => match spec.trim_start_matches('%').parse::<u32>() {
            Ok(id) => Some(id),
            Err(_) => {
                eprintln!("{}: no such job: {}", builtin, spec);
                None
            }
        },
        None => match JOB_LIST.lock().unwrap().current_id() {
            Some(id) => Some(id),
            None => {
                eprintln!("{}: current: no such job", builtin);
                None
            }
        },
    }
}

/// Resolve a redirection's slots against the descriptors the command would
/// otherwise have used.
fn resolve_slots(slots: &[edos_lib::process::StdioSlot; 3], defaults: [u64; 3]) -> [u64; 3] {
    [
        slots[0].resolve(defaults),
        slots[1].resolve(defaults),
        slots[2].resolve(defaults),
    ]
}

/// Run a builtin with descriptors 0, 1 and 2 pointing at `fds`, restoring the
/// shell's own descriptors afterwards.
///
/// Every save is taken before any `dup2`, so a redirection that reads a
/// descriptor it also replaces (`>file 2>&1`) still saves the original.
fn run_builtin_redirected(cmd: &str, args: &[String], fds: [u64; 3]) -> command::ExecResult {
    let mut saved: [Option<u64>; 3] = [None; 3];
    for (i, &fd) in fds.iter().enumerate() {
        if fd != i as u64 {
            saved[i] = edos_lib::process::dup(i as u64).ok();
        }
    }
    for (i, &fd) in fds.iter().enumerate() {
        if fd != i as u64 {
            let _ = edos_lib::process::dup2(fd, i as u64);
        }
    }

    let result = command::execute_command(cmd, args);
    let _ = std::io::stdout().flush();

    for (i, restore) in saved.iter().enumerate() {
        if let Some(restore) = *restore {
            let _ = edos_lib::process::dup2(restore, i as u64);
            edos_lib::process::close(restore);
        }
    }
    result
}

/// Feed `content` into a fresh pipe from a thread, and return the read end.
///
/// The write has to happen while the command is running, not before it starts.
/// A pipe holds a bounded amount and a writer blocks once it is full, so a
/// heredoc larger than that capacity would otherwise deadlock the shell
/// against itself: nothing is reading the pipe yet, and the shell is the thing
/// that would.
///
/// The thread owns the write end and closes it when done, which is what gives
/// the command its end of input.
pub fn heredoc_pipe(content: &str) -> Option<u64> {
    let (read_fd, write_fd) = edos_lib::process::pipe()?;
    let bytes = if content.is_empty() {
        Vec::new()
    } else {
        format!("{content}\n").into_bytes()
    };
    std::thread::spawn(move || {
        let mut sent = 0;
        while sent < bytes.len() {
            let Ok(n) = edos_lib::process::write(write_fd, &bytes[sent..]) else {
                break; // the reader went away; it will not miss what is left
            };
            if n == 0 {
                break; // the reader went away; it will not miss what is left
            }
            sent += n;
        }
        edos_lib::process::close(write_fd);
    });
    Some(read_fd)
}

/// Run a single command segment with a caller-supplied stdin fd.
///
/// The caller is responsible for closing `stdin_fd` after this returns.
/// This is used by heredoc execution so the pipe read end can be passed in
/// as the command's standard input.
pub fn run_segment_with_stdin(segment: &str, stdin_fd: u64) -> SegmentResult {
    let expanded = command::expand_variables(segment);
    let args = command::parse_command(&expanded);
    if args.is_empty() {
        return SegmentResult::Done(0);
    }
    let (first, rest) = args.split_first().unwrap();
    let cmd = &first.0;
    let (rest, redirects) = command::extract_redirects(rest);
    let rest = glob::expand_words(&rest);

    let Some(open) = command::open_redirects(&redirects) else {
        return SegmentResult::Done(1);
    };
    // The heredoc pipe is this command's standard input unless it redirects
    // its own.
    let fds = resolve_slots(&open.slots, [stdin_fd, 1, 2]);

    let code = if command::is_builtin(cmd) || crate::script::is_function(cmd) {
        match run_builtin_redirected(cmd, &rest, fds) {
            command::ExecResult::Success(c) | command::ExecResult::Failed(c) => c,
            command::ExecResult::Exit(code) => {
                open.close();
                return SegmentResult::Exit(code);
            }
        }
    } else {
        edos_lib::io::pty_set_canonical(0);
        let code = if let Some(pid) =
            edos_lib::process::spawn_program_with_fds(cmd, &rest, fds[0], fds[1], fds[2])
        {
            edos_lib::process::waitpid(pid)
        } else {
            eprintln!("Command not found: {}", cmd);
            127
        };
        edos_lib::io::pty_set_raw(0);
        code
    };

    open.close();
    SegmentResult::Done(code)
}

/// Check if a segment is a subshell expression `(...)` and return the inner content.
fn extract_subshell(s: &str) -> Option<&str> {
    let s = s.trim();
    if !s.starts_with('(') {
        return None;
    }
    let mut depth: usize = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    if i == s.len() - 1 {
                        return Some(&s[1..i]);
                    } else {
                        return None; // closing paren is not at end
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Run a full command chain (handles `&&`, `||`, `;`, `&` operators, and subshells).
///
/// Returns the exit code of the last command executed, or `Exit` carrying the
/// status the `exit` builtin asked for.
pub fn run_chain(input: &str) -> SegmentResult {
    // Strip comments before processing
    let input = command::strip_comment(input).to_string();
    let trimmed_input = input.trim();
    if trimmed_input.is_empty() {
        return SegmentResult::Done(0);
    }

    let chain = command::split_chain(&input);
    if chain.is_empty() {
        return SegmentResult::Done(0);
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

        // Determine whether this segment should run in the background.
        let background = *next_op == Some(command::ChainOp::Background);

        // Subshell: run the inner commands in a forked child.
        if let Some(inner) = extract_subshell(segment) {
            let pid = edos_lib::process::fork();
            if pid == Ok(0) {
                // A subshell is a process, so `exit` in it ends the subshell
                // with that status rather than the shell that forked it.
                let code = match run_chain(inner) {
                    SegmentResult::Done(code) | SegmentResult::Exit(code) => code,
                };
                std::process::exit(code);
            } else if let Ok(pid) = pid
                && pid > 0
            {
                if background {
                    let job_id = JOB_LIST.lock().unwrap().add(
                        vec![pid],
                        segment.to_string(),
                        jobs::JobStatus::Running,
                    );
                    command::set_last_bg_pid(pid);
                    println!("[{}] {}", job_id, pid);
                    last_exit = 0;
                    command::set_last_exit_code(0);
                } else {
                    let code = edos_lib::process::waitpid(pid);
                    last_exit = code;
                    command::set_last_exit_code(code);
                }
            }
            prev_op = *next_op;
            continue;
        }

        // Background execution.
        if background {
            last_exit = run_background(segment);
            command::set_last_exit_code(last_exit);
            prev_op = *next_op;
            continue;
        }

        match run_segment(segment) {
            SegmentResult::Done(code) => {
                last_exit = code;
                command::set_last_exit_code(code);
            }
            SegmentResult::Exit(code) => {
                return SegmentResult::Exit(code);
            }
        }

        prev_op = *next_op;
    }

    SegmentResult::Done(last_exit)
}

/// Total length in bytes of the UTF-8 sequence `lead` starts, or `None` when
/// `lead` cannot begin one (a stray continuation byte, or an encoding no longer
/// permitted by RFC 3629).
fn utf8_seq_len(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

/// Find the byte offset of the previous character boundary before `pos`.
fn prev_char_boundary(s: &str, pos: usize) -> usize {
    let mut i = pos - 1;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Find the byte offset of the next character boundary after `pos`.
fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut i = pos + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
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
fn redraw_from_cursor(line: &str, cursor: usize, _prompt_len: usize) {
    // Clear from cursor to end of line, print remaining, move cursor back
    let remaining = &line[cursor..];
    print!("\x1B[K{}", remaining);
    // Move cursor back by character count (not byte count)
    let chars_after = remaining.chars().count();
    if chars_after > 0 {
        print!("\x1B[{}D", chars_after);
    }
    let _ = std::io::stdout().flush();
}

/// Raw mode for as long as a line is being edited, and normal terminal
/// behaviour the rest of the time.
///
/// The shell echoes and positions the cursor itself while editing, which needs
/// the driver to stay out of the way. Everywhere else -- a prompt, a builtin's
/// output, an external command -- wants the ordinary cooked mode, because that
/// is what turns a newline into a carriage return and a newline. Without the
/// restore, every builtin that used `println!` drew a staircase on a real
/// terminal, and only `edos_render`'s own widget made it look right.
struct RawMode;

impl RawMode {
    fn enter() -> Self {
        edos_lib::io::pty_set_raw(0);
        Self
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        edos_lib::io::pty_set_canonical(0);
    }
}

fn read_line(history: &[String], prompt: &str) -> Option<String> {
    let _raw = RawMode::enter();
    // DECTCEM on: a full-screen program killed before it could restore the
    // cursor would otherwise leave it hidden for the rest of the session.
    print!("\x1B[?25h{}", prompt);
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    let mut cursor: usize = 0; // byte position in line
    let mut buf = [0u8; 1];
    let mut history_index = history.len();
    let mut saved_line = String::new();
    let prompt_len = prompt.len(); // approximate (ANSI codes make this inaccurate but OK)

    loop {
        let _poll_ready = poll_stdin(100);
        // EINTR from Ctrl+C or error: discard partial input, re-prompt.
        let n = sys_read(0, &mut buf).ok()?;
        if n == 0 {
            continue;
        }

        let ch = buf[0];

        if ch == b'\n' || ch == b'\r' {
            // Written in raw mode, so the carriage return is the shell's to
            // send: the driver is not translating while a line is being edited.
            print!("\r\n");
            let _ = std::io::stdout().flush();
            line.push('\n');
            return Some(line);
        }

        if ch == 0x08 || ch == 0x7F {
            if cursor > 0 {
                // Move cursor back to previous char boundary
                let prev = prev_char_boundary(&line, cursor);
                line.drain(prev..cursor);
                cursor = prev;
                // Move terminal cursor back, then redraw from there
                print!("\x08");
                redraw_from_cursor(&line, cursor, prompt_len);
            }
            continue;
        }

        if ch == 0x1B {
            let mut seq = [0u8; 2];
            if poll_stdin(20) {
                let n = sys_read(0, &mut seq[..1]);
                if n == Ok(1) && seq[0] == b'[' && poll_stdin(20) {
                    {
                        let n = sys_read(0, &mut seq[1..2]);
                        if n == Ok(1) {
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
                                        cursor = next_char_boundary(&line, cursor);
                                        print!("\x1B[C");
                                        let _ = std::io::stdout().flush();
                                    }
                                }
                                b'D' => {
                                    // Left arrow
                                    if cursor > 0 {
                                        cursor = prev_char_boundary(&line, cursor);
                                        print!("\x1B[D");
                                        let _ = std::io::stdout().flush();
                                    }
                                }
                                b'H' => {
                                    // Home
                                    if cursor > 0 {
                                        let chars_before = line[..cursor].chars().count();
                                        print!("\x1B[{}D", chars_before);
                                        cursor = 0;
                                        let _ = std::io::stdout().flush();
                                    }
                                }
                                // End
                                b'F' if cursor < line.len() => {
                                    let chars_after = line[cursor..].chars().count();
                                    print!("\x1B[{}C", chars_after);
                                    cursor = line.len();
                                    let _ = std::io::stdout().flush();
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
            let word_start = line_before_cursor.rfind(' ').map(|p| p + 1).unwrap_or(0);
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
                let chars_after = line[cursor..].chars().count();
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

        // Insert character at cursor position. A byte at or above 0x80 opens a
        // multi-byte UTF-8 sequence whose continuation bytes arrive on later
        // reads, so the whole sequence is decoded before anything is inserted.
        let ch = if ch < 0x80 {
            ch as char
        } else {
            let Some(seq_len) = utf8_seq_len(ch) else {
                continue;
            };
            let mut seq = [0u8; 4];
            seq[0] = ch;
            let mut got = 1;
            while got < seq_len {
                if !poll_stdin(20) {
                    break;
                }
                let n = sys_read(0, &mut seq[got..got + 1]);
                if n != Ok(1) {
                    break;
                }
                got += 1;
            }
            match std::str::from_utf8(&seq[..got])
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(c) => c,
                None => continue,
            }
        };
        let char_len = ch.len_utf8();
        if cursor == line.len() {
            // Append (common case)
            line.push(ch);
            cursor += char_len;
            print!("{}", ch);
            let _ = std::io::stdout().flush();
        } else {
            // Insert in middle
            line.insert(cursor, ch);
            cursor += char_len;
            // Print from insertion point to end, then move cursor back
            print!("{}", &line[cursor - char_len..]);
            let chars_after = line[cursor..].chars().count();
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
            std::env::set_var("HOME", "/home/edos");
        }
        if std::env::var("PWD").is_err()
            && let Ok(cwd) = std::env::current_dir()
        {
            std::env::set_var("PWD", cwd);
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
                let code = match run_chain(&cmd) {
                    SegmentResult::Done(code) | SegmentResult::Exit(code) => code,
                };
                std::process::exit(code);
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

    claim_terminal();

    // Raw mode is entered by `read_line` and left when it returns, so the
    // shell starts out cooked: the banner below, and every builtin's output,
    // get their newlines translated like any other program's.

    // Ignore SIGINT so the shell doesn't die on Ctrl+C
    // (the foreground child process gets killed instead)
    let _ = edos_lib::process::sys_sigaction(2, 1); // SIGINT=2, SIG_IGN=1

    // The system's release, not the shell's own: `/proc/version` is rendered
    // from the kernel's `CARGO_PKG_VERSION`, which is the one place it is set.
    let release = std::fs::read_to_string("/proc/version")
        .ok()
        .and_then(|v| v.split_whitespace().nth(1).map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    println!("EDOS {release} shell");
    println!("Type 'help' for commands.\n");

    let mut stdout = std::io::stdout();

    // Flush welcome message immediately so terminal can display it
    let _ = stdout.flush();

    let mut history: Vec<String> = Vec::new();
    // Load history from file. On the root filesystem rather than in /tmp,
    // which is memfs: history that dies with the boot is not history.
    if let Ok(content) = std::fs::read_to_string(config::SH_HISTORY) {
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
        // Reap completed background jobs and print notices
        let changed = JOB_LIST.lock().unwrap().reap();
        for (id, cmd, status) in changed {
            println!("[{}] {} {}", id, jobs::status_word(status), cmd);
        }

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
                .open(config::SH_HISTORY)
            {
                let _ = writeln!(f, "{}", trimmed);
            }
        }

        // Handle `history` builtin before splitting (needs access to history vec)
        if trimmed == "history" {
            builtins::cmd_history(&history);
            continue;
        }

        // Handle interactive function definition: collect lines until `end`
        if let Some(after) = trimmed.strip_prefix("function ") {
            let func_name = after.trim().to_string();
            if func_name.is_empty() {
                eprintln!("sh: function: missing name");
                continue;
            }
            let mut body_lines: Vec<String> = Vec::new();
            let mut depth: usize = 1;
            loop {
                let Some(body_input) = read_line(&history, "> ") else {
                    eprintln!("sh: unexpected EOF in function definition");
                    break;
                };
                let body_line = body_input.trim_end_matches('\n').to_string();
                let body_first = body_line.split_whitespace().next().unwrap_or("");
                match body_first {
                    "if" | "while" | "for" | "function" => depth += 1,
                    "end" => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                body_lines.push(body_line);
            }
            match script::parse_and_register_function(&func_name, &body_lines) {
                Ok(()) => {}
                Err(e) => eprintln!("sh: function {}: {}", func_name, e),
            }
            continue;
        }

        // Check for heredoc operator before running the command.
        let trimmed_check = input.trim().trim_end_matches('\n');
        if let Some((cleaned_line, marker, raw)) = command::parse_heredoc_marker(trimmed_check) {
            // Collect heredoc body lines interactively until the marker is seen.
            let mut heredoc_content: Vec<String> = Vec::new();
            while let Some(hline) = read_line(&[], "> ") {
                let hline_trimmed = hline.trim_end_matches('\n').to_string();
                if hline_trimmed.trim() == marker {
                    break;
                }
                heredoc_content.push(hline_trimmed);
            }
            let content = heredoc_content.join("\n");
            let expanded = if raw {
                content
            } else {
                command::expand_variables(&content)
            };

            if let Some(read_fd) = heredoc_pipe(&expanded) {
                let result = run_segment_with_stdin(&cleaned_line, read_fd);
                edos_lib::process::close(read_fd);
                match result {
                    SegmentResult::Done(code) => command::set_last_exit_code(code),
                    SegmentResult::Exit(code) => std::process::exit(code),
                }
            }
            continue;
        }

        if let SegmentResult::Exit(code) = run_chain(&input) {
            std::process::exit(code);
        }
    }
}
