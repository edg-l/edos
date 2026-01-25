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
            '\\' if in_quotes => {
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
