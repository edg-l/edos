use edos_lib::args::{Opt, Spec};
use std::fs;
use std::io::{self, BufRead};

const SPEC: Spec = Spec::new(
    "tail",
    "[-n N] [file...]",
    &[Opt::arg(
        'n',
        "lines",
        "N",
        "print the last N lines (the default, N=10)",
    )],
)
.numeric('n');

fn main() {
    let m = SPEC.parse_env();
    let n: usize = m.parsed('n').unwrap_or(10);
    let files: Vec<&String> = m
        .positional()
        .iter()
        .filter(|p| p.as_str() != "-")
        .collect();

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
