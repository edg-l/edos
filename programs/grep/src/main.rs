use edos_lib::args::{Opt, Spec};
use std::fs;
use std::io::{self, BufRead};

const SPEC: Spec = Spec::new(
    "grep",
    "[-cinv] PATTERN [file...]",
    &[
        Opt::flag('i', "ignore-case", "match without regard to case"),
        Opt::flag('v', "invert-match", "select the lines that do not match"),
        Opt::flag('n', "line-number", "prefix each line with its number"),
        Opt::flag('c', "count", "print only how many lines matched"),
    ],
);

fn main() {
    let m = SPEC.parse_env();
    let ignore_case = m.is_set('i');
    let invert = m.is_set('v');
    let line_numbers = m.is_set('n');
    let count_only = m.is_set('c');

    let operands = m.positional();
    if operands.is_empty() {
        SPEC.fail("no pattern given");
    }
    let pattern = &operands[0];
    let pattern_lower = pattern.to_lowercase();
    let files: Vec<&str> = operands[1..]
        .iter()
        .map(String::as_str)
        .filter(|p| *p != "-")
        .collect();
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
