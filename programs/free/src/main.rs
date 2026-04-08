//! free - show memory information from /proc/meminfo

use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let args = &args[1..];

    let human_readable = args.iter().any(|a| a == "-h");

    for arg in args {
        if arg != "-h" {
            eprintln!("Usage: free [-h]");
            process::exit(1);
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
                process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("free: failed to read /proc/meminfo: {}", e);
            process::exit(1);
        }
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
