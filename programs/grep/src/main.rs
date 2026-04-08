use std::env;
use std::fs;
use std::io::{self, BufRead};

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut ignore_case = false;
    let mut invert = false;
    let mut line_numbers = false;
    let mut count_only = false;
    let mut pattern_idx = 1;

    while pattern_idx < args.len() && args[pattern_idx].starts_with('-') {
        for c in args[pattern_idx][1..].chars() {
            match c {
                'i' => ignore_case = true,
                'v' => invert = true,
                'n' => line_numbers = true,
                'c' => count_only = true,
                _ => {
                    eprintln!("grep: unknown option -{}", c);
                    std::process::exit(1);
                }
            }
        }
        pattern_idx += 1;
    }

    if pattern_idx >= args.len() {
        eprintln!("Usage: grep [-ivnc] PATTERN [FILE...]");
        std::process::exit(1);
    }

    let pattern = &args[pattern_idx];
    let pattern_lower = pattern.to_lowercase();
    let files: Vec<&str> = args[pattern_idx + 1..].iter().map(|s| s.as_str()).collect();
    let multi_file = files.len() > 1;

    let matches_line = |line: &str| -> bool {
        let haystack = if ignore_case {
            line.to_lowercase()
        } else {
            line.to_string()
        };
        let needle = if ignore_case { &pattern_lower } else { pattern };
        let found = haystack.contains(needle.as_str());
        if invert { !found } else { found }
    };

    if files.is_empty() {
        // Read from stdin
        let stdin = io::stdin();
        let mut count = 0u64;
        for (i, line) in stdin.lock().lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if matches_line(&line) {
                count += 1;
                if !count_only {
                    if line_numbers {
                        print!("{}:", i + 1);
                    }
                    println!("{}", line);
                }
            }
        }
        if count_only {
            println!("{}", count);
        }
    } else {
        for file in &files {
            let content = match fs::read_to_string(file) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("grep: {}: {}", file, e);
                    continue;
                }
            };
            let mut count = 0u64;
            for (i, line) in content.lines().enumerate() {
                if matches_line(line) {
                    count += 1;
                    if !count_only {
                        if multi_file {
                            print!("{}:", file);
                        }
                        if line_numbers {
                            print!("{}:", i + 1);
                        }
                        println!("{}", line);
                    }
                }
            }
            if count_only {
                if multi_file {
                    print!("{}:", file);
                }
                println!("{}", count);
            }
        }
    }
}
