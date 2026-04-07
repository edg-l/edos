//! Tab completion for commands and file paths.

use std::env;
use std::fs;

/// Complete a command name by searching PATH directories and builtins.
pub fn complete_command(prefix: &str) -> Vec<String> {
    let mut matches = Vec::new();

    // Check builtins first
    let builtins = [
        "exit", "cd", "pwd", "clear", "echo", "export", "unset", "env", "history", "help",
    ];
    for b in &builtins {
        if b.starts_with(prefix) {
            matches.push(b.to_string());
        }
    }

    // Search PATH directories
    let path = env::var("PATH").unwrap_or_else(|_| "/bin".to_string());
    for dir in path.split(':') {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(prefix) && !matches.contains(&name) {
                    matches.push(name);
                }
            }
        }
    }

    matches.sort();
    matches
}

/// Complete a file path.
pub fn complete_path(prefix: &str) -> Vec<String> {
    let (dir, file_prefix) = if let Some(pos) = prefix.rfind('/') {
        (&prefix[..=pos], &prefix[pos + 1..])
    } else {
        (".", prefix)
    };

    let mut matches = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(file_prefix) {
                let full = if dir == "." {
                    name.clone()
                } else {
                    format!("{}{}", dir, name)
                };
                // Append / for directories
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    matches.push(format!("{}/", full));
                } else {
                    matches.push(full);
                }
            }
        }
    }

    matches.sort();
    matches
}

/// Find the longest common prefix among a slice of strings.
pub fn longest_common_prefix(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    let first = &strings[0];
    let mut len = first.len();
    for s in &strings[1..] {
        len = len.min(s.len());
        for (i, (a, b)) in first.bytes().zip(s.bytes()).enumerate() {
            if a != b {
                len = len.min(i);
                break;
            }
        }
    }
    first[..len].to_string()
}
