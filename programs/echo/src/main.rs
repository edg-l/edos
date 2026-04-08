//! echo - print arguments to stdout

use std::env;
use std::io::{self, Write};

fn main() {
    let args: Vec<String> = env::args().collect();
    let args = &args[1..];

    let output = if args.first().map(|s| s.as_str()) == Some("-e") {
        expand_escapes(&args[1..].join(" "))
    } else {
        args.join(" ")
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(output.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn expand_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('e') => out.push('\x1B'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}
