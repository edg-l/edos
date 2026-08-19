use std::env;
use std::fs;
use std::io::{self, BufRead, Write};

fn process_lines<I>(lines: I, count: bool, only_dup: bool, only_uniq: bool, out: &mut impl Write)
where
    I: Iterator<Item = String>,
{
    let mut current: Option<String> = None;
    let mut cur_count: usize = 0;

    let flush = |line: &str, n: usize, out: &mut dyn Write| {
        let print = if only_dup {
            n > 1
        } else if only_uniq {
            n == 1
        } else {
            true
        };
        if print {
            if count {
                let _ = writeln!(out, "{:>7} {}", n, line);
            } else {
                let _ = writeln!(out, "{}", line);
            }
        }
    };

    for line in lines {
        match &current {
            Some(cur) if *cur == line => {
                cur_count += 1;
            }
            _ => {
                if let Some(prev) = current.take() {
                    flush(&prev, cur_count, out);
                }
                cur_count = 1;
                current = Some(line);
            }
        }
    }
    if let Some(last) = current {
        flush(&last, cur_count, out);
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut count = false;
    let mut only_dup = false;
    let mut only_uniq = false;
    let mut file: Option<&str> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            arg if arg.starts_with('-') && arg.len() > 1 => {
                for c in arg[1..].chars() {
                    match c {
                        'c' => count = true,
                        'd' => only_dup = true,
                        'u' => only_uniq = true,
                        _ => {
                            eprintln!("uniq: unknown option -{}", c);
                            std::process::exit(1);
                        }
                    }
                }
            }
            _ => {
                if file.is_none() {
                    file = Some(&args[i]);
                }
            }
        }
        i += 1;
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    match file {
        Some(path) => {
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("uniq: {}: {}", path, e);
                    std::process::exit(1);
                }
            };
            let lines = content.lines().map(|l| l.to_string());
            process_lines(lines, count, only_dup, only_uniq, &mut out);
        }
        None => {
            let stdin = io::stdin();
            let lines = stdin.lock().lines().filter_map(|l| l.ok());
            process_lines(lines, count, only_dup, only_uniq, &mut out);
        }
    }
}
