//! Command parsing and dispatch.

use crate::builtins;
use crate::spawn;

/// Expand `$VAR` and `${VAR}` references in a string segment.
///
/// Rules:
/// - `$$` expands to a literal `$`
/// - `${VAR}` and `$VAR` expand to the environment value (empty string if unset)
/// - Characters inside single-quoted regions are left unexpanded
/// - Characters inside double-quoted regions are expanded
/// - `$?` is left as-is (not supported)
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
pub fn parse_command(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '\"';
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
            }
            q if in_quotes && q == quote_char => {
                in_quotes = false;
            }
            '\\' => {
                // Backslash escapes the next character (inside or outside quotes)
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            ' ' | '\t' | '\n' | '\r' if !in_quotes => {
                if !current.is_empty() {
                    args.push(expand_tilde(&current));
                    current = String::new();
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        args.push(expand_tilde(&current));
    }

    args
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

/// Parsed redirections from a command line.
#[derive(Default)]
pub struct Redirects {
    /// File to redirect stdin from (`< file`)
    pub stdin_file: Option<String>,
    /// File to redirect stdout to (`> file` or `>> file`)
    pub stdout_file: Option<String>,
    /// Whether stdout redirect is append mode (`>>`)
    pub stdout_append: bool,
}

/// Extract `>`, `>>`, `<` redirections from args, returning remaining args and redirects.
pub fn extract_redirects(args: &[String]) -> (Vec<String>, Redirects) {
    let mut remaining = Vec::new();
    let mut redirects = Redirects::default();
    let mut i = 0;

    while i < args.len() {
        if args[i] == ">" || args[i] == ">>" {
            redirects.stdout_append = args[i] == ">>";
            if i + 1 < args.len() {
                redirects.stdout_file = Some(args[i + 1].clone());
                i += 2;
            } else {
                eprintln!("syntax error: expected filename after {}", args[i]);
                i += 1;
            }
        } else if args[i] == "<" {
            if i + 1 < args.len() {
                redirects.stdin_file = Some(args[i + 1].clone());
                i += 2;
            } else {
                eprintln!("syntax error: expected filename after <");
                i += 1;
            }
        } else if args[i].starts_with(">>") {
            redirects.stdout_append = true;
            redirects.stdout_file = Some(args[i][2..].to_string());
            i += 1;
        } else if args[i].starts_with('>') {
            redirects.stdout_append = false;
            redirects.stdout_file = Some(args[i][1..].to_string());
            i += 1;
        } else if args[i].starts_with('<') {
            redirects.stdin_file = Some(args[i][1..].to_string());
            i += 1;
        } else {
            remaining.push(args[i].clone());
            i += 1;
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
}

/// Split input into command chains on unquoted `&&`, `||`, and `;`.
/// Returns pairs of (command_string, operator_after) where the last has None.
pub fn split_chain(input: &str) -> Vec<(String, Option<ChainOp>)> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';
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
            '&' if !in_quotes && chars.peek() == Some(&'&') => {
                chars.next(); // consume second '&'
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    result.push((trimmed, Some(ChainOp::And)));
                }
                current = String::new();
            }
            '|' if !in_quotes && chars.peek() == Some(&'|') => {
                chars.next(); // consume second '|'
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    result.push((trimmed, Some(ChainOp::Or)));
                }
                current = String::new();
            }
            ';' if !in_quotes => {
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
        "exit" | "help" | "pwd" | "cd" | "clear" | "echo" | "export" | "unset" | "env" | "history"
    )
}

/// Command execution result.
pub enum ExecResult {
    /// Command ran (builtins always succeed for now).
    Ok,
    /// Shell should exit.
    Exit,
    /// Command not found.
    NotFound,
}

/// Execute a command.
pub fn execute_command(command: &str, args: &[String]) -> ExecResult {
    match command {
        "exit" => return ExecResult::Exit,
        "help" => builtins::cmd_help(),
        "pwd" => builtins::cmd_pwd(),
        "cd" => builtins::cmd_cd(args),
        "clear" => builtins::cmd_clear(),
        "echo" => builtins::cmd_echo(args),
        "export" => builtins::cmd_export(args),
        "unset" => builtins::cmd_unset(args),
        "env" => builtins::cmd_env(),
        _ => {
            // Restore canonical mode for child (echo + line buffering)
            edos_lib::io::pty_set_canonical(0);
            if let Some(pid) = spawn::spawn_program_with_fds(command, args, 0, 1, 2) {
                edos_lib::process::waitpid(pid);
            } else {
                eprintln!("Command not found: {}", command);
                edos_lib::io::pty_set_raw(0);
                return ExecResult::NotFound;
            }
            // Back to raw mode for shell's own line editing
            edos_lib::io::pty_set_raw(0);
        }
    }
    ExecResult::Ok
}
