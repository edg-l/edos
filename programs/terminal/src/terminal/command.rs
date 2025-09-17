use alloc::{format, string::String, vec::Vec};
use elibc::{
    io::{FileType, chdir, getcwd, list_dir, open, open_flags, read_to_end, write_all_fd},
    pipe, spawn, sys_close,
};

use super::state::{Program, TerminalState};

pub(super) fn parse_command(input: &str) -> Vec<String> {
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
            ' ' | '\t' if !in_quotes => {
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

pub(super) fn execute_command(state: &mut TerminalState, command: &str, args: &[String]) {
    match command {
        "logs" => toggle_logs(state, args),
        "help" => print_help(state),
        "pwd" => print_pwd(state),
        "cd" => change_directory(state, args),
        "ls" => list_directory(state, args),
        "cat" => cat_file(state, args),
        "write" => write_file(state, args),
        "clear" => state.clear_output(),
        _ => spawn_program(state, command, args),
    }
}

fn toggle_logs(state: &mut TerminalState, args: &[String]) {
    if let Some(mode) = args.first().map(String::as_str) {
        match mode {
            "on" => state.set_logs_enabled(true),
            "off" => state.set_logs_enabled(false),
            _ => state.write_line("Usage: logs [on|off]"),
        }
    } else {
        state.write_line("Usage: logs [on|off]");
    }
}

fn print_help(state: &mut TerminalState) {
    let lines = [
        "Commands:",
        "- help",
        "- logs",
        "- pwd",
        "- cd [path]",
        "- ls [path]",
        "- cat <path>",
        "- write <path> <content>",
        "- clear",
    ];

    for line in lines {
        state.write_line(line);
    }
}

fn print_pwd(state: &mut TerminalState) {
    match getcwd() {
        Ok(cwd) => state.write_line(&cwd),
        Err(_) => state.write_line("pwd: error getting current directory"),
    }
}

fn change_directory(state: &mut TerminalState, args: &[String]) {
    let target = args.first().map(String::as_str).unwrap_or("/");

    match chdir(target) {
        Ok(()) => state.update_prompt(),
        Err(_) => state.write_line(&format!("cd: {}: No such file or directory", target)),
    }
}

fn list_directory(state: &mut TerminalState, args: &[String]) {
    let path = args.first().map(String::as_str).unwrap_or(".");

    match list_dir(path) {
        Ok(entries) if entries.is_empty() => {}
        Ok(entries) => {
            let mut line = String::new();
            for (idx, entry) in entries.iter().enumerate() {
                let suffix = match entry.file_type {
                    FileType::File => "",
                    FileType::Directory => "/",
                    FileType::Symlink => "@",
                    FileType::Special => "*",
                };

                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(&entry.name);
                line.push_str(suffix);

                if (idx + 1) % 12 == 0 {
                    state.write_line(&line);
                    line.clear();
                }
            }
            if !line.is_empty() {
                state.write_line(&line);
            }
        }
        Err(_) => state.write_line(&format!(
            "ls: cannot access '{}': No such file or directory",
            path
        )),
    }
}

fn cat_file(state: &mut TerminalState, args: &[String]) {
    let Some(path) = args.first() else {
        state.write_line("Usage: cat <path>");
        return;
    };

    match open(path, 0) {
        Ok(fd) => {
            match read_to_end(fd, Some(16 * 1024)) {
                Ok(data) => match core::str::from_utf8(&data) {
                    Ok(text) => {
                        state.write_str(text);
                        state.write_line("");
                    }
                    Err(_) => state.write_line("[non-utf8 data]"),
                },
                Err(_) => state.write_line("cat: read error"),
            }
            let _ = sys_close(fd);
        }
        Err(_) => state.write_line("cat: open failed"),
    }
}

fn write_file(state: &mut TerminalState, args: &[String]) {
    if args.len() < 2 {
        state.write_line("Usage: write <path> <content>");
        return;
    }

    let path = &args[0];
    let content = args[1..].join(" ");

    match open(path, open_flags::O_APPEND | open_flags::O_CREAT) {
        Ok(fd) => {
            match write_all_fd(fd, content.as_bytes()) {
                Ok(()) => {}
                Err(_) => state.write_line("write: error"),
            }
            let _ = sys_close(fd);
        }
        Err(_) => state.write_line("write: open failed"),
    }
}

fn spawn_program(state: &mut TerminalState, command: &str, args: &[String]) {
    let Some((read_fd, write_fd)) = pipe() else {
        state.write_line(&format!("Failed to create pipe for {command}"));
        return;
    };

    let mut argv: Vec<&str> = Vec::with_capacity(args.len() + 1);
    for arg in args {
        argv.push(arg);
    }

    let candidates = [
        format!("/bin/{}", command),
        format!("./{}", command),
        format!("/usr/bin/{}", command),
        format!("/{}", command),
    ];

    for path in candidates.iter() {
        let pid = spawn(path, &argv, 0, write_fd, 2);
        if pid != u64::MAX {
            let _ = sys_close(write_fd);
            state.set_running_program(Some(Program { pid, read_fd }));
            return;
        }
    }

    state.write_line(&format!("Command not found: {}", command));
    let _ = sys_close(read_fd);
    let _ = sys_close(write_fd);
}
