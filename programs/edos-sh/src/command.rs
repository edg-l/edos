//! Command parsing and dispatch.

use crate::builtins;
use crate::spawn;
use edos_lib::process::{STDIO_DEFAULT, StdioSlot};

// ---------------------------------------------------------------------------
// Positional parameters ($0, $1, ..., $#, $@) and last exit code ($?)
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

/// The last exit code, updated after every command runs.
static LAST_EXIT_CODE: AtomicI32 = AtomicI32::new(0);

/// Exit-on-error flag (set -e / set +e).
static EXIT_ON_ERROR: AtomicBool = AtomicBool::new(false);

/// PID of the last background job (`$!`).
static LAST_BG_PID: AtomicU64 = AtomicU64::new(0);

/// Set the exit-on-error flag.
pub fn set_exit_on_error(val: bool) {
    EXIT_ON_ERROR.store(val, Ordering::Relaxed);
}

/// Get the current exit-on-error flag value.
pub fn exit_on_error() -> bool {
    EXIT_ON_ERROR.load(Ordering::Relaxed)
}

/// Return the PID of the last background job (used for `$!` expansion).
pub fn last_bg_pid() -> u64 {
    LAST_BG_PID.load(Ordering::Relaxed)
}

/// Set the PID of the last background job.
pub fn set_last_bg_pid(pid: u64) {
    LAST_BG_PID.store(pid, Ordering::Relaxed);
}

/// Update the last exit code.
pub fn set_last_exit_code(code: i32) {
    LAST_EXIT_CODE.store(code, Ordering::Relaxed);
}

/// Return the last exit code (used for `$?` expansion).
pub fn last_exit_code() -> i32 {
    LAST_EXIT_CODE.load(Ordering::Relaxed)
}

/// Script/shell positional arguments ($0, $1, ...).
static SCRIPT_ARGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Set positional parameters (call once at startup with argv).
pub fn set_script_args(args: &[String]) {
    *SCRIPT_ARGS.lock().unwrap() = args.to_vec();
}

/// Return the nth positional parameter, or None if out of range.
pub fn get_script_arg(n: usize) -> Option<String> {
    SCRIPT_ARGS.lock().unwrap().get(n).cloned()
}

/// Return the number of positional parameters excluding $0.
pub fn script_arg_count() -> usize {
    SCRIPT_ARGS.lock().unwrap().len().saturating_sub(1)
}

/// Return all positional parameters except $0, space-joined (for `$@`).
pub fn script_args_all() -> String {
    let args = SCRIPT_ARGS.lock().unwrap();
    if args.len() > 1 {
        args[1..].join(" ")
    } else {
        String::new()
    }
}

/// Return all positional parameters (including $0) as a Vec.
pub fn get_all_script_args() -> Vec<String> {
    SCRIPT_ARGS.lock().unwrap().clone()
}

/// Capture the stdout of a shell command by running it in a subprocess with a pipe.
/// Trailing newlines are stripped from the result.
fn capture_command_output(cmd: &str) -> String {
    let (read_fd, write_fd) = match edos_lib::process::pipe() {
        Some(fds) => fds,
        None => return String::new(),
    };

    // Parse cmd into program + args
    let parts = parse_command_simple(cmd);
    if parts.is_empty() {
        edos_lib::process::close(read_fd);
        edos_lib::process::close(write_fd);
        return String::new();
    }
    let program = &parts[0];
    let args: Vec<String> = parts[1..].to_vec();

    let pid = spawn::spawn_program_with_fds(program, &args, 0, write_fd, 2);

    // Parent closes write end so read end gets EOF when child exits
    edos_lib::process::close(write_fd);

    let output = match pid {
        Some(child_pid) => {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 512];
            loop {
                let n = edos_lib::process::read(read_fd, &mut chunk);
                if n <= 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n as usize]);
            }
            edos_lib::process::close(read_fd);
            edos_lib::process::waitpid(child_pid);
            let s = String::from_utf8_lossy(&buf).into_owned();
            // Strip trailing newlines
            s.trim_end_matches('\n').to_string()
        }
        None => {
            edos_lib::process::close(read_fd);
            String::new()
        }
    };

    output
}

/// Expand `$VAR`, `${VAR}`, `$(cmd)`, and special parameters in a string segment.
///
/// Rules:
/// - `$$` expands to a literal `$`
/// - `${VAR}` and `$VAR` expand to the environment value (empty string if unset)
/// - `$(cmd)` expands to the stdout of cmd with trailing newlines stripped
/// - `$?` expands to the last command's exit code
/// - `$0`–`$9` expand to positional parameters
/// - `$#` expands to the count of positional parameters (excluding `$0`)
/// - `$@` expands to all positional parameters space-joined (excluding `$0`)
/// - Characters inside single-quoted regions are left unexpanded
/// - Characters inside double-quoted regions are expanded
pub fn expand_variables(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_single_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                in_single_quote = !in_single_quote;
                // Don't include the quote character itself in expanded output;
                // the caller (parse_command) handles quoting around token boundaries.
                // We push it so parse_command's quote-stripping still works correctly.
                out.push(ch);
            }
            '$' if !in_single_quote => {
                match chars.peek() {
                    Some(&'$') => {
                        chars.next();
                        out.push('$');
                    }
                    Some(&'?') => {
                        chars.next();
                        out.push_str(&last_exit_code().to_string());
                    }
                    Some(&'#') => {
                        chars.next();
                        out.push_str(&script_arg_count().to_string());
                    }
                    Some(&'@') => {
                        chars.next();
                        out.push_str(&script_args_all());
                    }
                    Some(&'!') => {
                        chars.next();
                        let pid = last_bg_pid();
                        if pid > 0 {
                            out.push_str(&pid.to_string());
                        }
                    }
                    Some(&d) if d.is_ascii_digit() => {
                        chars.next();
                        let n = (d as u8 - b'0') as usize;
                        let val = get_script_arg(n).unwrap_or_default();
                        out.push_str(&val);
                    }
                    Some(&'(') => {
                        chars.next(); // consume first '('
                        // Check for arithmetic expansion $((expr))
                        if chars.peek() == Some(&'(') {
                            chars.next(); // consume second '('
                            // Collect until matching '))'
                            let mut expr = String::new();
                            let mut depth = 0u32;
                            loop {
                                match chars.next() {
                                    None => break,
                                    Some(')') if depth == 0 => {
                                        // Need another ')' to close '))'
                                        if chars.peek() == Some(&')') {
                                            chars.next();
                                            break;
                                        } else {
                                            expr.push(')');
                                        }
                                    }
                                    Some('(') => {
                                        depth += 1;
                                        expr.push('(');
                                    }
                                    Some(')') => {
                                        depth = depth.saturating_sub(1);
                                        expr.push(')');
                                    }
                                    Some(c) => expr.push(c),
                                }
                            }
                            // Expand shell variables ($1, $VAR, etc.) before arithmetic eval
                            let expanded_expr = expand_variables(&expr);
                            match crate::arith::eval_arithmetic(&expanded_expr) {
                                Ok(val) => out.push_str(&val.to_string()),
                                Err(e) => eprintln!("sh: arithmetic error: {}", e),
                            }
                        } else {
                            // Command substitution: collect inner command tracking paren depth and quotes
                            // so nested $() and quoted parens are handled correctly.
                            let mut inner = String::new();
                            let mut depth: usize = 1;
                            let mut sq = false; // inside single quotes
                            let mut dq = false; // inside double quotes
                            for c in chars.by_ref() {
                                match c {
                                    '\'' if !dq => sq = !sq,
                                    '"' if !sq => dq = !dq,
                                    '(' if !sq && !dq => {
                                        depth += 1;
                                        inner.push(c);
                                    }
                                    ')' if !sq && !dq => {
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                        inner.push(c);
                                    }
                                    _ => inner.push(c),
                                }
                            }
                            let result = capture_command_output(&inner);
                            out.push_str(&result);
                        }
                    }
                    Some(&'{') => {
                        chars.next(); // consume '{'
                        let mut name = String::new();
                        for c in chars.by_ref() {
                            if c == '}' {
                                break;
                            }
                            name.push(c);
                        }
                        let val = std::env::var(&name).unwrap_or_default();
                        out.push_str(&val);
                    }
                    Some(&c) if c.is_alphanumeric() || c == '_' => {
                        let mut name = String::new();
                        while let Some(&c) = chars.peek() {
                            if c.is_alphanumeric() || c == '_' {
                                name.push(c);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        let val = std::env::var(&name).unwrap_or_default();
                        out.push_str(&val);
                    }
                    _ => {
                        // Bare `$` with no recognizable expansion — pass through
                        out.push('$');
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Remove a `#` comment from a line.
///
/// A `#` starts a comment when it appears at the start of the line or is
/// preceded by whitespace, and is not inside single or double quotes.
/// Returns the portion of the line before the comment, with trailing
/// whitespace trimmed.
pub fn strip_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_was_space = true; // treat start-of-line as "preceded by space"
    let bytes = line.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double && prev_was_space => {
                return line[..i].trim_end();
            }
            _ => {}
        }
        prev_was_space = !in_single && !in_double && (b == b' ' || b == b'\t');
    }
    line
}

/// Expand a leading `~` to the HOME directory in a path token.
pub fn expand_tilde(input: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    if input == "~" {
        home
    } else if let Some(rest) = input.strip_prefix("~/") {
        format!("{}/{}", home, rest)
    } else {
        input.to_string()
    }
}

/// Parse a command line into arguments, handling quotes and escapes.
/// Parse a command line into tokens, tracking whether each token was (even partially) quoted.
/// The bool in each tuple is true if any part of the token was inside quotes.
pub fn parse_command(input: &str) -> Vec<(String, bool)> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '\"';
    let mut was_quoted = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
                was_quoted = true;
            }
            q if in_quotes && q == quote_char => {
                in_quotes = false;
            }
            '\\' => {
                // Backslash escapes the next character (inside or outside
                // quotes), which makes the word literal for the same reasons a
                // quote does: no redirection operator, no pattern.
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                    was_quoted = true;
                }
            }
            ' ' | '\t' | '\n' | '\r' if !in_quotes => {
                if !current.is_empty() {
                    args.push((expand_tilde(&current), was_quoted));
                    current = String::new();
                    was_quoted = false;
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        args.push((expand_tilde(&current), was_quoted));
    }

    args
}

/// Parse a command line into plain strings (discarding quote info).
pub fn parse_command_simple(input: &str) -> Vec<String> {
    parse_command(input)
        .into_iter()
        .map(|(text, _)| text)
        .collect()
}

/// Split input into pipeline stages on unquoted `|` characters.
pub fn split_pipeline(input: &str) -> Vec<String> {
    let mut stages = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';

    for ch in input.chars() {
        match ch {
            '"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
                current.push(ch);
            }
            q if in_quotes && q == quote_char => {
                in_quotes = false;
                current.push(ch);
            }
            '|' if !in_quotes => {
                stages.push(current.trim().to_string());
                current = String::new();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        stages.push(trimmed);
    }
    stages
}

/// How a redirection opens its file.
#[derive(Clone, Copy, PartialEq)]
pub enum RedirMode {
    /// `<`
    Read,
    /// `>`
    Write,
    /// `>>`
    Append,
}

impl RedirMode {
    /// Open flags: `0x1` O_WRONLY, `0x40` O_CREAT, `0x200` O_TRUNC, `0x400` O_APPEND.
    fn open_flags(self) -> u64 {
        match self {
            RedirMode::Read => 0,
            RedirMode::Write => 0x1 | 0x40 | 0x200,
            RedirMode::Append => 0x1 | 0x40 | 0x400,
        }
    }
}

/// A single redirection, kept in the order it was written.
pub enum RedirOp {
    /// `N<file`, `N>file`, `N>>file`
    File {
        fd: usize,
        path: String,
        mode: RedirMode,
    },
    /// `N>&M`: descriptor N takes wherever M is wired at this point in the line.
    Dup { fd: usize, from: usize },
}

/// Parsed redirections from a command line, in written order.
#[derive(Default)]
pub struct Redirects {
    pub ops: Vec<RedirOp>,
}

/// A redirection list with its files opened.
pub struct OpenRedirects {
    /// Where descriptors 0, 1 and 2 end up.
    pub slots: [StdioSlot; 3],
    /// Descriptors opened here, which the caller closes once the command is done.
    opened: Vec<u64>,
}

impl OpenRedirects {
    /// True when the command runs on the caller's own descriptors.
    pub fn is_default(&self) -> bool {
        self.slots == STDIO_DEFAULT
    }

    /// Close every file this opened.
    pub fn close(self) {
        for fd in self.opened {
            edos_lib::process::close(fd);
        }
    }
}

/// Open every file a redirection list names, in the order it was written.
///
/// `2>&1` copies whatever descriptor 1 is wired to *at that point*, so
/// `>f 2>&1` sends both streams to the file while `2>&1 >f` leaves the error
/// stream on the terminal.
pub fn open_redirects(redirects: &Redirects) -> Option<OpenRedirects> {
    let mut open = OpenRedirects {
        slots: STDIO_DEFAULT,
        opened: Vec::new(),
    };
    for op in &redirects.ops {
        match op {
            RedirOp::File { fd, path, mode } => {
                let opened = edos_lib::io::open(path, mode.open_flags());
                if opened < 0 {
                    if *mode == RedirMode::Read {
                        eprintln!("{}: cannot open for reading", path);
                    } else {
                        eprintln!("{}: cannot open for writing", path);
                    }
                    open.close();
                    return None;
                }
                open.opened.push(opened as u64);
                open.slots[*fd] = StdioSlot::Fd(opened as u64);
            }
            RedirOp::Dup { fd, from } => open.slots[*fd] = open.slots[*from],
        }
    }
    Some(open)
}

/// A redirection operator found at the head of a token.
struct RedirOperator {
    /// Descriptor being redirected.
    fd: usize,
    mode: RedirMode,
    /// `&>`: standard output and error together.
    both: bool,
    /// `>&` / `<&`: the target names a descriptor, not a file.
    dup: bool,
    /// The target when it is part of the same token; empty when it is the next one.
    target: String,
}

/// Parse the redirection operator at the start of `tok`.
///
/// Returns `None` when the token does not begin with one, which leaves a word
/// like `x>y` an ordinary argument.
fn parse_redirect_token(tok: &str) -> Option<RedirOperator> {
    let bytes = tok.as_bytes();
    let (fd, both, mut i) = if tok.starts_with("&>") {
        (1, true, 1)
    } else {
        let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
        let op = *bytes.get(digits)?;
        if op != b'<' && op != b'>' {
            return None;
        }
        let fd = if digits == 0 {
            if op == b'<' { 0 } else { 1 }
        } else {
            tok[..digits].parse().ok()?
        };
        (fd, false, digits)
    };

    let op = bytes[i];
    i += 1;
    let mut mode = if op == b'<' {
        RedirMode::Read
    } else {
        RedirMode::Write
    };
    if op == b'>' && bytes.get(i) == Some(&b'>') {
        mode = RedirMode::Append;
        i += 1;
    }
    let dup = bytes.get(i) == Some(&b'&');
    if dup {
        i += 1;
    }

    Some(RedirOperator {
        fd,
        mode,
        both,
        dup,
        target: tok[i..].to_string(),
    })
}

/// Detect `<<MARKER` or `<< MARKER` in a command line.
///
/// Returns `Some((line_without_heredoc, marker, raw))` where:
/// - `line_without_heredoc` is the line with the `<<MARKER` portion removed
/// - `marker` is the delimiter string
/// - `raw` is true when the marker was quoted with single quotes (no variable expansion)
///
/// Returns `None` if no heredoc operator is found.
/// `<<<` (herestring) is explicitly excluded.
pub fn parse_heredoc_marker(line: &str) -> Option<(String, String, bool)> {
    let bytes = line.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => {
                in_single = !in_single;
                i += 1;
            }
            b'"' if !in_single => {
                in_double = !in_double;
                i += 1;
            }
            b'<' if !in_single && !in_double => {
                // Need at least two characters: <<
                if i + 1 < bytes.len() && bytes[i + 1] == b'<' {
                    // Reject <<< (herestring)
                    if i + 2 < bytes.len() && bytes[i + 2] == b'<' {
                        i += 1;
                        continue;
                    }
                    // Found `<<` at position i
                    let before = line[..i].trim_end().to_string();
                    let after = line[i + 2..].trim_start();

                    // Parse marker: may be single-quoted
                    let (marker, raw) = if after.starts_with('\'') {
                        // Single-quoted marker: strip quotes
                        let rest = &after[1..];
                        if let Some(close) = rest.find('\'') {
                            (rest[..close].to_string(), true)
                        } else {
                            // Unclosed quote: treat entire rest as marker
                            (rest.to_string(), true)
                        }
                    } else {
                        // Unquoted marker: take until whitespace
                        let marker = after.split_whitespace().next().unwrap_or("").to_string();
                        (marker, false)
                    };

                    if marker.is_empty() {
                        return None;
                    }

                    return Some((before, marker, raw));
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Extract redirections from args, returning the remaining words -- still
/// carrying their quoted flag, since pathname expansion needs it -- and the
/// redirection list.
///
/// Understood: `<`, `>`, `>>`, an explicit descriptor number in front of any
/// of them (`2>file`), descriptor duplication (`2>&1`), and `&>` / `&>>` for
/// both output streams. Tokens that were quoted during parsing are never
/// treated as redirect operators. Only descriptors 0, 1 and 2 can be
/// redirected, since those are the three a spawned program is given.
pub fn extract_redirects(args: &[(String, bool)]) -> (Vec<(String, bool)>, Redirects) {
    let mut remaining = Vec::new();
    let mut redirects = Redirects::default();
    let mut i = 0;

    while i < args.len() {
        let (ref tok, quoted) = args[i];
        let op = if quoted {
            None
        } else {
            parse_redirect_token(tok)
        };
        let Some(op) = op else {
            remaining.push((tok.clone(), quoted));
            i += 1;
            continue;
        };
        i += 1;

        let target = if op.target.is_empty() {
            match args.get(i) {
                Some((next, _)) => {
                    i += 1;
                    next.clone()
                }
                None => {
                    eprintln!("sh: syntax error: {} needs a target", tok);
                    continue;
                }
            }
        } else {
            op.target.clone()
        };

        if op.fd > 2 {
            eprintln!("sh: {}: only descriptors 0, 1 and 2 can be redirected", tok);
            continue;
        }

        if op.dup {
            match target.parse::<usize>() {
                Ok(from) if from <= 2 => redirects.ops.push(RedirOp::Dup { fd: op.fd, from }),
                _ => eprintln!("sh: {}: bad file descriptor", target),
            }
        } else {
            redirects.ops.push(RedirOp::File {
                fd: op.fd,
                path: target,
                mode: op.mode,
            });
        }

        // `&>file` is `>file 2>&1`: one open description shared by both
        // streams, so the two do not overwrite each other's output.
        if op.both {
            redirects.ops.push(RedirOp::Dup { fd: 2, from: 1 });
        }
    }

    (remaining, redirects)
}

/// A conditional operator between commands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChainOp {
    /// `&&` -- run next only if previous succeeded
    And,
    /// `||` -- run next only if previous failed
    Or,
    /// `;` -- run next unconditionally
    Semi,
    /// `&` -- run in background
    Background,
}

/// Split input into command chains on unquoted `&&`, `||`, `;`, and `&`.
/// Returns pairs of (command_string, operator_after) where the last has None.
/// Paren depth is tracked so operators inside `(...)` are not recognized.
pub fn split_chain(input: &str) -> Vec<(String, Option<ChainOp>)> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';
    let mut paren_depth: usize = 0;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
                current.push(ch);
            }
            q if in_quotes && q == quote_char => {
                in_quotes = false;
                current.push(ch);
            }
            '(' if !in_quotes => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' if !in_quotes => {
                paren_depth = paren_depth.saturating_sub(1);
                current.push(ch);
            }
            // An `&` belonging to a redirection (`2>&1`, `&>file`) is not the
            // background operator.
            '&' if !in_quotes
                && paren_depth == 0
                && !current.ends_with(['>', '<'])
                && chars.peek() != Some(&'>') =>
            {
                if chars.peek() == Some(&'&') {
                    chars.next(); // consume second '&'
                    let trimmed = current.trim().to_string();
                    if !trimmed.is_empty() {
                        result.push((trimmed, Some(ChainOp::And)));
                    }
                    current = String::new();
                } else {
                    // Single '&': background operator
                    let trimmed = current.trim().to_string();
                    if !trimmed.is_empty() {
                        result.push((trimmed, Some(ChainOp::Background)));
                    }
                    current = String::new();
                }
            }
            '|' if !in_quotes && paren_depth == 0 && chars.peek() == Some(&'|') => {
                chars.next(); // consume second '|'
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    result.push((trimmed, Some(ChainOp::Or)));
                }
                current = String::new();
            }
            ';' if !in_quotes && paren_depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    result.push((trimmed, Some(ChainOp::Semi)));
                }
                current = String::new();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        result.push((trimmed, None));
    }
    result
}

/// Check if a command is a builtin.
pub fn is_builtin(command: &str) -> bool {
    matches!(
        command,
        "exit"
            | "help"
            | "pwd"
            | "cd"
            | "clear"
            | "echo"
            | "export"
            | "unset"
            | "env"
            | "history"
            | "test"
            | "["
            | "break"
            | "continue"
            | "set"
            | "return"
            | "kill"
            | "ifconfig"
            | "ip"
            | "jobs"
            | "wait"
    )
}

/// Command execution result.
pub enum ExecResult {
    /// Command ran successfully with the given exit code.
    Success(i32),
    /// Command failed with the given exit code (non-zero), or was not found (127).
    Failed(i32),
    /// Shell should exit.
    Exit,
}

/// Execute a command.
pub fn execute_command(command: &str, args: &[String]) -> ExecResult {
    match command {
        "exit" => return ExecResult::Exit,
        "help" => {
            builtins::cmd_help();
            return ExecResult::Success(0);
        }
        "pwd" => {
            builtins::cmd_pwd();
            return ExecResult::Success(0);
        }
        "cd" => {
            builtins::cmd_cd(args);
            return ExecResult::Success(0);
        }
        "clear" => {
            builtins::cmd_clear();
            return ExecResult::Success(0);
        }
        "echo" => {
            builtins::cmd_echo(args);
            return ExecResult::Success(0);
        }
        "export" => {
            builtins::cmd_export(args);
            return ExecResult::Success(0);
        }
        "unset" => {
            builtins::cmd_unset(args);
            return ExecResult::Success(0);
        }
        "env" => {
            builtins::cmd_env();
            return ExecResult::Success(0);
        }
        "test" => {
            let code = builtins::cmd_test(args);
            return if code == 0 {
                ExecResult::Success(0)
            } else {
                ExecResult::Failed(code)
            };
        }
        "[" => {
            // Strip trailing ']' argument
            let test_args: Vec<String> =
                args.iter().filter(|a| a.as_str() != "]").cloned().collect();
            let code = builtins::cmd_test(&test_args);
            return if code == 0 {
                ExecResult::Success(0)
            } else {
                ExecResult::Failed(code)
            };
        }
        "break" => {
            // Flow control handled by the script executor; return success here
            // so the executor can detect "break" was the command.
            return ExecResult::Success(0);
        }
        "continue" => {
            // Flow control handled by the script executor; return success here
            // so the executor can detect "continue" was the command.
            return ExecResult::Success(0);
        }
        "return" => {
            // Flow control handled by the script executor.
            return ExecResult::Success(0);
        }
        "set" => {
            match args.first().map(|s| s.as_str()) {
                Some("-e") => set_exit_on_error(true),
                Some("+e") => set_exit_on_error(false),
                _ => {} // Ignore unrecognized set flags
            }
            return ExecResult::Success(0);
        }
        "kill" => {
            let code = builtins::cmd_kill(args);
            return if code == 0 {
                ExecResult::Success(0)
            } else {
                ExecResult::Failed(code)
            };
        }
        "jobs" => {
            crate::JOB_LIST.lock().unwrap().print();
            return ExecResult::Success(0);
        }
        "wait" => {
            let code = if let Some(pid_str) = args.first() {
                match pid_str.parse::<u64>() {
                    Ok(pid) => crate::JOB_LIST.lock().unwrap().wait_pid(pid),
                    Err(_) => {
                        eprintln!("wait: invalid PID: {}", pid_str);
                        1
                    }
                }
            } else {
                crate::JOB_LIST.lock().unwrap().wait_all()
            };
            return if code == 0 {
                ExecResult::Success(0)
            } else {
                ExecResult::Failed(code)
            };
        }
        "ifconfig" | "ip" => {
            builtins::cmd_ifconfig(args);
            return ExecResult::Success(0);
        }
        _ if command.contains('=') && !command.starts_with('=') => {
            // Bare VAR=value assignment
            if let Some(eq_pos) = command.find('=') {
                let key = &command[..eq_pos];
                // Validate: key must be a valid identifier (alphanumeric + underscore, not starting with digit)
                if !key.is_empty()
                    && key.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && !key.starts_with(|c: char| c.is_ascii_digit())
                {
                    let val = &command[eq_pos + 1..];
                    unsafe { std::env::set_var(key, val) };
                    return ExecResult::Success(0);
                }
            }
            // Not a valid assignment, fall through to external program
            edos_lib::io::pty_set_canonical(0);
            if let Some(pid) = spawn::spawn_program_with_fds(command, args, 0, 1, 2) {
                let code = edos_lib::process::waitpid(pid);
                edos_lib::io::pty_set_raw(0);
                return if code == 0 {
                    ExecResult::Success(0)
                } else {
                    ExecResult::Failed(code)
                };
            } else {
                eprintln!("Command not found: {}", command);
                edos_lib::io::pty_set_raw(0);
                return ExecResult::Failed(127);
            }
        }
        _ if crate::script::is_function(command) => {
            let code = crate::script::call_function(command, args);
            return if code == 0 {
                ExecResult::Success(0)
            } else {
                ExecResult::Failed(code)
            };
        }
        _ => {
            // Restore canonical mode for child (echo + line buffering)
            edos_lib::io::pty_set_canonical(0);
            if let Some(pid) = spawn::spawn_program_with_fds(command, args, 0, 1, 2) {
                let code = edos_lib::process::waitpid(pid);
                edos_lib::io::pty_set_raw(0);
                return if code == 0 {
                    ExecResult::Success(0)
                } else {
                    ExecResult::Failed(code)
                };
            } else {
                eprintln!("Command not found: {}", command);
                edos_lib::io::pty_set_raw(0);
                return ExecResult::Failed(127);
            }
        }
    }
}
