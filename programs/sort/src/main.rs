use edos_lib::args::{Opt, Spec};
use std::fs;
use std::io::{self, BufRead, Write};

const SPEC: Spec = Spec::new(
    "sort",
    "[-nru] [file...]",
    &[
        Opt::flag('n', "numeric-sort", "compare lines as numbers"),
        Opt::flag('r', "reverse", "reverse the result of the comparison"),
        Opt::flag('u', "unique", "print only the first of an equal run"),
    ],
);

fn main() {
    let m = SPEC.parse_env();
    let reverse = m.is_set('r');
    let numeric = m.is_set('n');
    let unique = m.is_set('u');
    let files: Vec<&str> = m
        .positional()
        .iter()
        .map(String::as_str)
        .filter(|p| *p != "-")
        .collect();

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
