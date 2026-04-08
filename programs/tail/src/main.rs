use std::env;
use std::fs;
use std::io::{self, BufRead};

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut n: usize = 10;
    let mut files: Vec<&str> = Vec::new();
    let mut i = 1;

    while i < args.len() {
        if args[i] == "-n" && i + 1 < args.len() {
            n = args[i + 1].parse().unwrap_or(10);
            i += 2;
        } else if args[i].starts_with('-') && args[i].len() > 1 {
            n = args[i][1..].parse().unwrap_or(10);
            i += 1;
        } else {
            files.push(&args[i]);
            i += 1;
        }
    }

    let multi = files.len() > 1;

    if files.is_empty() {
        let stdin = io::stdin();
        let lines: Vec<String> = stdin.lock().lines().map_while(Result::ok).collect();
        let start = lines.len().saturating_sub(n);
        for line in &lines[start..] {
            println!("{}", line);
        }
    } else {
        for (fi, file) in files.iter().enumerate() {
            if multi {
                if fi > 0 {
                    println!();
                }
                println!("==> {} <==", file);
            }
            match fs::read_to_string(file) {
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = lines.len().saturating_sub(n);
                    for line in &lines[start..] {
                        println!("{}", line);
                    }
                }
                Err(e) => eprintln!("tail: {}: {}", file, e),
            }
        }
    }
}
