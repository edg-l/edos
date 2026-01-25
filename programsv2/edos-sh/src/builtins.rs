//! Built-in shell commands.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};

/// Print help message with available commands.
pub fn cmd_help() {
    println!("Commands:");
    println!("  help              - Show this help");
    println!("  pwd               - Print working directory");
    println!("  cd [path]         - Change directory");
    println!("  ls [path]         - List directory");
    println!("  cat <path>        - Print file contents");
    println!("  write <path> ...  - Append text to file");
    println!("  stat <path>       - Show file metadata");
    println!("  free [-h]         - Show memory info");
    println!("  ps [path]         - Show process list");
    println!("  dmesg [path]      - Show kernel log");
    println!("  mkdir <path>      - Create directory");
    println!("  rmdir <path>      - Remove empty directory");
    println!("  rm [-r] <path>    - Remove file or directory");
    println!("  clear             - Clear screen");
    println!("  echo [args...]    - Print arguments");
    println!("  exit              - Exit shell");
}

/// Print current working directory.
pub fn cmd_pwd() {
    match std::env::current_dir() {
        Ok(path) => println!("{}", path.display()),
        Err(e) => eprintln!("pwd: error getting current directory: {}", e),
    }
}

/// Change current directory.
pub fn cmd_cd(args: &[String]) {
    let target = args.first().map(String::as_str).unwrap_or("/");

    if let Err(e) = std::env::set_current_dir(target) {
        eprintln!("cd: {}: {}", target, e);
    }
}

/// List directory contents.
pub fn cmd_ls(args: &[String]) {
    let path = args.first().map(String::as_str).unwrap_or(".");

    match fs::read_dir(path) {
        Ok(entries) => {
            let mut items: Vec<_> = entries
                .filter_map(|e| e.ok())
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let suffix = if let Ok(ft) = entry.file_type() {
                        if ft.is_dir() {
                            "/"
                        } else if ft.is_symlink() {
                            "@"
                        } else {
                            ""
                        }
                    } else {
                        ""
                    };
                    format!("{}{}", name, suffix)
                })
                .collect();

            items.sort();

            // Print in columns (12 per line)
            let mut line = String::new();
            for (idx, item) in items.iter().enumerate() {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(item);

                if (idx + 1) % 12 == 0 {
                    println!("{}", line);
                    line.clear();
                }
            }
            if !line.is_empty() {
                println!("{}", line);
            }
        }
        Err(e) => eprintln!("ls: cannot access '{}': {}", path, e),
    }
}

/// Print file contents.
pub fn cmd_cat(args: &[String]) {
    let Some(path) = args.first() else {
        eprintln!("Usage: cat <path>");
        return;
    };

    match fs::read_to_string(path) {
        Ok(contents) => {
            print!("{}", contents);
            if !contents.ends_with('\n') {
                println!();
            }
        }
        Err(e) => eprintln!("cat: {}: {}", path, e),
    }
}

/// Append text to file.
pub fn cmd_write(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: write <path> <content>");
        return;
    }

    let path = &args[0];
    let content = args[1..].join(" ");

    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut file) => {
            if let Err(e) = file.write_all(content.as_bytes()) {
                eprintln!("write: error writing to {}: {}", path, e);
            }
        }
        Err(e) => eprintln!("write: cannot open '{}': {}", path, e),
    }
}

/// Show file metadata.
pub fn cmd_stat(args: &[String]) {
    let Some(path) = args.first() else {
        eprintln!("Usage: stat <path>");
        return;
    };

    match fs::metadata(path) {
        Ok(meta) => {
            println!("File: {}", path);

            let file_type = if meta.is_dir() {
                "Directory"
            } else if meta.is_symlink() {
                "Symbolic link"
            } else {
                "Regular file"
            };
            println!("Type: {}", file_type);
            println!("Size: {} bytes", meta.len());

            // Note: timestamps may not be available on EDOS
            if let Ok(modified) = meta.modified() {
                println!("Modified: {:?}", modified);
            }
        }
        Err(e) => eprintln!("stat: cannot stat '{}': {}", path, e),
    }
}

/// Show memory information from /proc/meminfo.
pub fn cmd_free(args: &[String]) {
    let human_readable = args.iter().any(|a| a == "-h");

    // Validate arguments
    for arg in args {
        if arg != "-h" {
            eprintln!("Usage: free [-h]");
            return;
        }
    }

    match fs::read_to_string("/proc/meminfo") {
        Ok(text) => {
            if let Some(info) = parse_meminfo(&text) {
                if human_readable {
                    println!("             total        used        free");
                } else {
                    println!("             total(KiB)  used(KiB)  free(KiB)");
                }
                println!(
                    "Mem:       {} {} {}",
                    format_kib(info.total_kib, human_readable),
                    format_kib(info.used_kib, human_readable),
                    format_kib(info.free_kib, human_readable)
                );
                println!(
                    "Frames:    {} {} {}",
                    format_count(info.frames_total, human_readable),
                    format_count(info.frames_used, human_readable),
                    format_count(info.frames_free, human_readable)
                );
                println!(
                    "Page size: {}",
                    format_bytes(info.page_size_bytes, human_readable)
                );
            } else {
                eprintln!("free: failed to parse meminfo, raw output follows:");
                print!("{}", text);
            }
        }
        Err(e) => eprintln!("free: failed to read /proc/meminfo: {}", e),
    }
}

struct ParsedMeminfo {
    total_kib: u64,
    free_kib: u64,
    used_kib: u64,
    page_size_bytes: u64,
    frames_total: u64,
    frames_free: u64,
    frames_used: u64,
}

fn parse_meminfo(text: &str) -> Option<ParsedMeminfo> {
    let mut total_kib = None;
    let mut free_kib = None;
    let mut used_kib = None;
    let mut page_size_bytes = None;
    let mut frames_total = None;
    let mut frames_free = None;
    let mut frames_used = None;

    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(label) = parts.next() else { continue };
        let Some(value_str) = parts.next() else {
            continue;
        };
        let value = match value_str.parse::<u64>() {
            Ok(val) => val,
            Err(_) => continue,
        };

        match label {
            "MemTotal:" => total_kib = Some(value),
            "MemFree:" => free_kib = Some(value),
            "MemUsed:" => used_kib = Some(value),
            "PageSize:" => page_size_bytes = Some(value),
            "FramesTotal:" => frames_total = Some(value),
            "FramesFree:" => frames_free = Some(value),
            "FramesUsed:" => frames_used = Some(value),
            _ => {}
        }
    }

    let total_kib = total_kib?;
    let free_kib = free_kib?;
    let used_kib = used_kib.unwrap_or_else(|| total_kib.saturating_sub(free_kib));
    let frames_total = frames_total?;
    let frames_free = frames_free?;
    let frames_used = frames_used.unwrap_or_else(|| frames_total.saturating_sub(frames_free));
    let page_size_bytes = page_size_bytes.unwrap_or(4096);

    Some(ParsedMeminfo {
        total_kib,
        free_kib,
        used_kib,
        page_size_bytes,
        frames_total,
        frames_free,
        frames_used,
    })
}

fn format_kib(value: u64, human: bool) -> String {
    if !human {
        return format!("{:>10}", value);
    }

    let text = format_bytes(value * 1024, true);
    format!("{:>12}", text)
}

fn format_count(value: u64, human: bool) -> String {
    if !human {
        return format!("{:>10}", value);
    }

    if value < 1000 {
        return format!("{:>10}", value);
    }

    let units = ["", "K", "M", "G", "T", "P"];
    let mut v = value as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit + 1 < units.len() {
        v /= 1000.0;
        unit += 1;
    }

    let text = format!("{:.1} {}", v, units[unit]);
    format!("{:>10}", text)
}

fn format_bytes(bytes: u64, human: bool) -> String {
    if !human {
        return format!("{} B", bytes);
    }

    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;

    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// Show process list from /proc/processes.
pub fn cmd_ps(args: &[String]) {
    let path = args.first().map(String::as_str).unwrap_or("/proc/processes");

    match fs::read_to_string(path) {
        Ok(text) => {
            print!("{}", text);
            if !text.ends_with('\n') {
                println!();
            }
        }
        Err(e) => eprintln!("ps: failed to open {}: {}", path, e),
    }
}

/// Show kernel log from /dev/klog.
pub fn cmd_dmesg(args: &[String]) {
    let path = args.first().map(String::as_str).unwrap_or("/dev/klog");

    // Read as bytes since klog might be large
    let mut buffer = Vec::new();
    match File::open(path) {
        Ok(file) => {
            // Limit read to 128KB
            let mut limited = file.take(128 * 1024);
            match limited.read_to_end(&mut buffer) {
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buffer);
                    print!("{}", text);
                    if !text.ends_with('\n') {
                        println!();
                    }
                }
                Err(e) => eprintln!("dmesg: read error: {}", e),
            }
        }
        Err(e) => eprintln!("dmesg: failed to open {}: {}", path, e),
    }
}

/// Create a directory.
pub fn cmd_mkdir(args: &[String]) {
    let Some(path) = args.first() else {
        eprintln!("Usage: mkdir <path>");
        return;
    };

    match fs::create_dir(path) {
        Ok(()) => println!("Directory created."),
        Err(e) => eprintln!("mkdir: cannot create '{}': {}", path, e),
    }
}

/// Remove an empty directory.
pub fn cmd_rmdir(args: &[String]) {
    let Some(path) = args.first() else {
        eprintln!("Usage: rmdir <path>");
        return;
    };

    match fs::remove_dir(path) {
        Ok(()) => println!("Directory removed."),
        Err(e) => eprintln!("rmdir: cannot remove '{}': {}", path, e),
    }
}

/// Remove file or directory.
pub fn cmd_rm(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: rm [-r] <path>");
        return;
    }

    let (recursive, path_arg) = if let Some(first) = args.first() {
        if first == "-r" || first == "-R" {
            (true, args.get(1))
        } else {
            (false, Some(first))
        }
    } else {
        (false, None)
    };

    let Some(path) = path_arg else {
        eprintln!("Usage: rm [-r] <path>");
        return;
    };

    let result = if recursive {
        // Try remove_dir_all first, fall back to remove_file
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(_) => fs::remove_file(path),
        }
    } else {
        fs::remove_file(path)
    };

    match result {
        Ok(()) => println!("Removed."),
        Err(e) => eprintln!("rm: cannot remove '{}': {}", path, e),
    }
}

/// Clear the terminal screen.
pub fn cmd_clear() {
    // ANSI escape sequence to clear screen and move cursor to home
    print!("\x1B[2J\x1B[H");
    let _ = std::io::stdout().flush();
}

/// Echo arguments to stdout.
pub fn cmd_echo(args: &[String]) {
    println!("{}", args.join(" "));
}
