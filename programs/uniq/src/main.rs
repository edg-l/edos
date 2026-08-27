use edos_lib::args::{Opt, Spec};
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

const SPEC: Spec = Spec::new(
    "uniq",
    "[-cdu] [file]",
    &[
        Opt::flag(
            'c',
            "count",
            "prefix each line with how many times it repeated",
        ),
        Opt::flag('d', "repeated", "print only lines that repeated"),
        Opt::flag('u', "unique", "print only lines that did not repeat"),
    ],
);

fn main() {
    let m = SPEC.parse_env();
    let count = m.is_set('c');
    let only_dup = m.is_set('d');
    let only_uniq = m.is_set('u');
    let file = m.positional().first().filter(|p| p.as_str() != "-");

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
            let lines = stdin.lock().lines().map_while(Result::ok);
            process_lines(lines, count, only_dup, only_uniq, &mut out);
        }
    }
}
