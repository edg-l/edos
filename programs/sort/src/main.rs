use std::env;
use std::fs;
use std::io::{self, BufRead, Write};

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut reverse = false;
    let mut numeric = false;
    let mut unique = false;
    let mut files: Vec<&str> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-r" => reverse = true,
            "-n" => numeric = true,
            "-u" => unique = true,
            "-rn" | "-nr" => {
                reverse = true;
                numeric = true;
            }
            "-ru" | "-ur" => {
                reverse = true;
                unique = true;
            }
            "-nu" | "-un" => {
                numeric = true;
                unique = true;
            }
            "-rnu" | "-run" | "-nru" | "-nur" | "-urn" | "-unr" => {
                reverse = true;
                numeric = true;
                unique = true;
            }
            arg if arg.starts_with('-') => {
                for c in arg[1..].chars() {
                    match c {
                        'r' => reverse = true,
                        'n' => numeric = true,
                        'u' => unique = true,
                        _ => {
                            eprintln!("sort: unknown option -{}", c);
                            std::process::exit(1);
                        }
                    }
                }
            }
            _ => files.push(&args[i]),
        }
        i += 1;
    }

    let mut lines: Vec<String> = Vec::new();

    if files.is_empty() {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => lines.push(l),
                Err(_) => break,
            }
        }
    } else {
        for file in &files {
            match fs::read_to_string(file) {
                Ok(content) => {
                    for line in content.lines() {
                        lines.push(line.to_string());
                    }
                }
                Err(e) => {
                    eprintln!("sort: {}: {}", file, e);
                    std::process::exit(1);
                }
            }
        }
    }

    lines.sort_by(|a, b| {
        if numeric {
            let na = a.trim().parse::<f64>();
            let nb = b.trim().parse::<f64>();
            match (na, nb) {
                (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(a.cmp(b)),
                _ => a.cmp(b),
            }
        } else {
            a.cmp(b)
        }
    });

    if reverse {
        lines.reverse();
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut prev: Option<&str> = None;
    for line in &lines {
        if unique {
            if prev == Some(line.as_str()) {
                continue;
            }
            prev = Some(line.as_str());
        }
        if out.write_all(line.as_bytes()).is_err() {
            break;
        }
        if out.write_all(b"\n").is_err() {
            break;
        }
    }
}
