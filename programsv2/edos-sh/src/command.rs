//! Command parsing and dispatch.

use crate::builtins;
use crate::spawn;

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
                    args.push(current);
                    current = String::new();
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        args.push(current);
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

/// Check if a command is a builtin.
pub fn is_builtin(command: &str) -> bool {
    matches!(
        command,
        "exit"
            | "help"
            | "pwd"
            | "cd"
            | "ls"
            | "cat"
            | "write"
            | "stat"
            | "free"
            | "ps"
            | "dmesg"
            | "mkdir"
            | "rmdir"
            | "rm"
            | "clear"
            | "echo"
    )
}

/// Execute a command. Returns false if shell should exit.
pub fn execute_command(command: &str, args: &[String]) -> bool {
    match command {
        "exit" => return false,
        "help" => builtins::cmd_help(),
        "pwd" => builtins::cmd_pwd(),
        "cd" => builtins::cmd_cd(args),
        "ls" => builtins::cmd_ls(args),
        "cat" => builtins::cmd_cat(args),
        "write" => builtins::cmd_write(args),
        "stat" => builtins::cmd_stat(args),
        "free" => builtins::cmd_free(args),
        "ps" => builtins::cmd_ps(args),
        "dmesg" => builtins::cmd_dmesg(args),
        "mkdir" => builtins::cmd_mkdir(args),
        "rmdir" => builtins::cmd_rmdir(args),
        "rm" => builtins::cmd_rm(args),
        "clear" => builtins::cmd_clear(),
        "echo" => builtins::cmd_echo(args),
        _ => spawn::spawn_program(command, args),
    }
    true
}
