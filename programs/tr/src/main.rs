use std::env;
use std::io::{self, Read, Write};

fn expand_set(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => result.push(b'\n'),
                b't' => result.push(b'\t'),
                b'\\' => result.push(b'\\'),
                c => result.push(c),
            }
            i += 2;
        } else if i + 2 < bytes.len() && bytes[i + 1] == b'-' {
            let start = bytes[i];
            let end = bytes[i + 2];
            if start <= end {
                for c in start..=end {
                    result.push(c);
                }
            }
            i += 3;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    result
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut delete = false;
    let mut squeeze = false;
    let mut arg_idx = 1;

    while arg_idx < args.len() && args[arg_idx].starts_with('-') && args[arg_idx].len() > 1 {
        for c in args[arg_idx][1..].chars() {
            match c {
                'd' => delete = true,
                's' => squeeze = true,
                _ => {
                    eprintln!("tr: unknown option -{}", c);
                    std::process::exit(1);
                }
            }
        }
        arg_idx += 1;
    }

    if arg_idx >= args.len() {
        eprintln!("usage: tr [-d] [-s] set1 [set2]");
        std::process::exit(1);
    }

    let set1 = expand_set(&args[arg_idx]);
    let set2 = if !delete && arg_idx + 1 < args.len() {
        expand_set(&args[arg_idx + 1])
    } else {
        Vec::new()
    };

    // Build translation table: byte -> Option<u8> (None = delete)
    let mut table: [Option<u8>; 256] = [None; 256];
    // Initialize identity
    for b in 0u8..=255 {
        table[b as usize] = Some(b);
    }

    if delete {
        for &b in &set1 {
            table[b as usize] = None;
        }
    } else {
        for (i, &b) in set1.iter().enumerate() {
            let mapped = if set2.is_empty() {
                b
            } else {
                *set2.get(i).unwrap_or_else(|| set2.last().unwrap())
            };
            table[b as usize] = Some(mapped);
        }
    }

    let mut input = Vec::new();
    if let Err(e) = io::stdin().read_to_end(&mut input) {
        eprintln!("tr: read: {}", e);
        std::process::exit(1);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut last_out: Option<u8> = None;

    for &b in &input {
        match table[b as usize] {
            None => {}
            Some(mapped) => {
                if squeeze && set2.contains(&mapped) {
                    if last_out == Some(mapped) {
                        continue;
                    }
                }
                if out.write_all(&[mapped]).is_err() {
                    break;
                }
                last_out = Some(mapped);
            }
        }
    }
}
