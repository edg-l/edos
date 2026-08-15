use std::env;
use std::fs;
use std::io::{self, BufRead, Read, Write};

/// How much of each input to print: whole lines, or a byte prefix.
#[derive(Clone, Copy)]
enum Limit {
    Lines(usize),
    Bytes(usize),
}

fn print_lines(text: &str, n: usize) {
    for line in text.lines().take(n) {
        println!("{}", line);
    }
}

/// Write a byte prefix verbatim: `-c` counts bytes, so it must not go through
/// a line-oriented path that would add or drop a newline.
fn print_bytes(bytes: &[u8], n: usize) {
    let end = n.min(bytes.len());
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(&bytes[..end]);
    let _ = out.flush();
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut limit = Limit::Lines(10);
    let mut files: Vec<&str> = Vec::new();
    let mut i = 1;

    while i < args.len() {
        if (args[i] == "-n" || args[i] == "-c") && i + 1 < args.len() {
            let count = args[i + 1].parse().unwrap_or(10);
            limit = if args[i] == "-c" {
                Limit::Bytes(count)
            } else {
                Limit::Lines(count)
            };
            i += 2;
        } else if let Some(rest) = args[i].strip_prefix("-c").filter(|r| !r.is_empty()) {
            limit = Limit::Bytes(rest.parse().unwrap_or(10));
            i += 1;
        } else if args[i].starts_with('-') && args[i].len() > 1 {
            let digits = args[i].strip_prefix("-n").unwrap_or(&args[i][1..]);
            limit = Limit::Lines(digits.parse().unwrap_or(10));
            i += 1;
        } else {
            files.push(&args[i]);
            i += 1;
        }
    }

    let multi = files.len() > 1;

    if files.is_empty() {
        match limit {
            Limit::Lines(n) => {
                let stdin = io::stdin();
                for line in stdin.lock().lines().take(n).map_while(Result::ok) {
                    println!("{}", line);
                }
            }
            Limit::Bytes(n) => {
                // `Read::read` returns at most `n` and routinely returns less:
                // a pipe hands over whatever has been written so far. Taking
                // one read would print however much happened to have arrived,
                // so read until `n` bytes or EOF. `take` also bounds the
                // allocation to what the input really holds, which keeps a
                // large `-c` from asking for the whole number up front.
                let mut buf = Vec::new();
                let _ = io::stdin().take(n as u64).read_to_end(&mut buf);
                print_bytes(&buf, n);
            }
        }
    } else {
        for (fi, file) in files.iter().enumerate() {
            if multi {
                if fi > 0 {
                    println!();
                }
                println!("==> {} <==", file);
            }
            match limit {
                Limit::Lines(n) => match fs::read_to_string(file) {
                    Ok(content) => print_lines(&content, n),
                    Err(e) => eprintln!("head: {}: {}", file, e),
                },
                Limit::Bytes(n) => match fs::read(file) {
                    Ok(content) => print_bytes(&content, n),
                    Err(e) => eprintln!("head: {}: {}", file, e),
                },
            }
        }
    }
}
