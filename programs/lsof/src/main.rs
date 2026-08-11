//! lsof - which threads have what open
//!
//! "Which process still holds this open" is the first question an unmount
//! failure or a file manager asks, and until `/proc/<tid>/fd` existed nothing
//! could answer it. That file is a table rather than the directory of symbolic
//! links Linux offers, because half the descriptors in this system have no
//! path: a pipe end, a PTY side and a socket are identified by the address of
//! the object they share and by their endpoints.
//!
//! The kernel's unit is the thread, so a process with several threads reports
//! its descriptors once per thread. That is the truth procfs holds, and it is
//! left visible rather than collapsed.

use std::fs;
use std::process::ExitCode;

use edos_lib::procinfo;

/// One row of `/proc/<tid>/fd`, plus the thread it came from.
struct OpenFile {
    command: String,
    tid: u64,
    fd: u64,
    kind: String,
    mode: String,
    pos: u64,
    name: String,
}

struct Options {
    /// Print nothing but the thread ids, for `kill $(lsof -t FILE)`.
    terse: bool,
    /// Restrict to one thread id.
    tid: Option<u64>,
    /// Restrict to threads whose command contains this.
    command: Option<String>,
    /// Restrict to descriptors naming one of these paths, or anything under
    /// them. Empty means every descriptor.
    paths: Vec<String>,
}

fn usage() -> ! {
    eprintln!("usage: lsof [-t] [-p TID] [-c COMMAND] [FILE...]");
    std::process::exit(2)
}

/// Absolute form of an operand, so `lsof .` and `lsof /tmp` compare against
/// the absolute paths the kernel reports.
fn absolute(path: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_string());
        if cwd == "/" {
            format!("/{path}")
        } else {
            format!("{cwd}/{path}")
        }
    };

    // A trailing slash would break the "under this directory" comparison,
    // which appends its own.
    match joined.strip_suffix('/') {
        Some(trimmed) if !trimmed.is_empty() => trimmed.to_string(),
        _ => joined,
    }
}

fn parse_args() -> Options {
    let mut options = Options {
        terse: false,
        tid: None,
        command: None,
        paths: Vec::new(),
    };
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" => options.terse = true,
            "-p" => match args.next().and_then(|value| value.parse().ok()) {
                Some(tid) => options.tid = Some(tid),
                None => usage(),
            },
            "-c" => match args.next() {
                Some(value) => options.command = Some(value),
                None => usage(),
            },
            "-h" | "--help" => usage(),
            other if other.starts_with('-') && other.len() > 1 => usage(),
            other => options.paths.push(absolute(other)),
        }
    }

    options
}

/// `FD TYPE MODE POS NAME`, with NAME taking the rest of the line because a
/// socket's is several tokens.
fn parse_row(line: &str, command: &str, tid: u64) -> Option<OpenFile> {
    let mut fields = line.split_whitespace();
    let fd = fields.next()?.parse().ok()?;
    let kind = fields.next()?.to_string();
    let mode = fields.next()?.to_string();
    let pos = fields.next()?.parse().ok()?;
    let name = fields.collect::<Vec<_>>().join(" ");
    Some(OpenFile {
        command: command.to_string(),
        tid,
        fd,
        kind,
        mode,
        pos,
        name,
    })
}

fn matches_paths(file: &OpenFile, paths: &[String]) -> bool {
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|path| {
        file.name == *path
            || file
                .name
                .starts_with(&format!("{}/", path.trim_end_matches('/')))
    })
}

fn collect(options: &Options) -> Result<Vec<OpenFile>, String> {
    let table = procinfo::read_table().map_err(|err| format!("lsof: /proc/processes: {err}"))?;
    let mut files = Vec::new();

    for process in &table.processes {
        if options.tid.is_some_and(|tid| tid != process.pid) {
            continue;
        }
        if options
            .command
            .as_ref()
            .is_some_and(|needle| !process.name.contains(needle.as_str()))
        {
            continue;
        }

        // A thread that exits between the table read and this one simply has
        // nothing to report; so does a kernel thread, which owns no table.
        let Ok(text) = fs::read_to_string(format!("/proc/{}/fd", process.pid)) else {
            continue;
        };

        for line in text.lines().skip(1) {
            if let Some(file) = parse_row(line, &process.name, process.pid)
                && matches_paths(&file, &options.paths)
            {
                files.push(file);
            }
        }
    }

    Ok(files)
}

fn print_table(files: &[OpenFile]) {
    let command_width = files
        .iter()
        .map(|file| file.command.len())
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<command_width$} {:>5} {:>3} {:<6} {:<4} {:>10} NAME",
        "COMMAND", "TID", "FD", "TYPE", "MODE", "POS"
    );
    for file in files {
        println!(
            "{:<command_width$} {:>5} {:>3} {:<6} {:<4} {:>10} {}",
            file.command, file.tid, file.fd, file.kind, file.mode, file.pos, file.name
        );
    }
}

fn main() -> ExitCode {
    let options = parse_args();

    let files = match collect(&options) {
        Ok(files) => files,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(1);
        }
    };

    if options.terse {
        // Each thread once, in the order first seen, so the output feeds
        // straight into `kill`.
        let mut seen: Vec<u64> = Vec::new();
        for file in &files {
            if !seen.contains(&file.tid) {
                seen.push(file.tid);
                println!("{}", file.tid);
            }
        }
    } else {
        print_table(&files);
    }

    // Matching nothing is a failure when the caller asked about a specific
    // file, which is what makes `lsof FILE && echo busy` work.
    if files.is_empty() && !options.paths.is_empty() {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
