use edos_lib::args::{Opt, Spec};
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

const SPEC: Spec = Spec::new(
    "head",
    "[-n N | -c N] [file...]",
    &[
        Opt::arg(
            'n',
            "lines",
            "N",
            "print the first N lines (the default, N=10)",
        ),
        Opt::arg('c', "bytes", "N", "print the first N bytes"),
    ],
)
.numeric('n');

fn main() {
    let m = SPEC.parse_env();
    let mut limit = Limit::Lines(10);
    for (opt, value) in m.occurrences() {
        let n = value.unwrap_or("").parse().unwrap_or_else(|_| {
            SPEC.fail(&format!("invalid count: {}", value.unwrap_or("")));
        });
        limit = match opt.short {
            Some('c') => Limit::Bytes(n),
            _ => Limit::Lines(n),
        };
    }
    let files: Vec<&String> = m
        .positional()
        .iter()
        .filter(|p| p.as_str() != "-")
        .collect();

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
