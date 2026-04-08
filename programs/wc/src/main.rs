use std::env;
use std::fs;
use std::io::{self, Read};

fn count(content: &str) -> (usize, usize, usize) {
    let lines = content.lines().count();
    let words = content.split_whitespace().count();
    let bytes = content.len();
    (lines, words, bytes)
}

fn print_counts(lines: usize, words: usize, bytes: usize, name: &str, show: (bool, bool, bool)) {
    if show.0 {
        print!("{:>8}", lines);
    }
    if show.1 {
        print!("{:>8}", words);
    }
    if show.2 {
        print!("{:>8}", bytes);
    }
    if !name.is_empty() {
        print!(" {}", name);
    }
    println!();
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut show_lines = false;
    let mut show_words = false;
    let mut show_bytes = false;
    let mut files: Vec<&str> = Vec::new();

    for arg in &args[1..] {
        if arg.starts_with('-') && arg.len() > 1 {
            for c in arg[1..].chars() {
                match c {
                    'l' => show_lines = true,
                    'w' => show_words = true,
                    'c' => show_bytes = true,
                    _ => {
                        eprintln!("wc: unknown option -{}", c);
                        std::process::exit(1);
                    }
                }
            }
        } else {
            files.push(arg);
        }
    }

    // Default: show all
    if !show_lines && !show_words && !show_bytes {
        show_lines = true;
        show_words = true;
        show_bytes = true;
    }
    let show = (show_lines, show_words, show_bytes);

    let mut total = (0usize, 0usize, 0usize);

    if files.is_empty() {
        let mut content = String::new();
        if io::stdin().read_to_string(&mut content).is_ok() {
            let (l, w, b) = count(&content);
            print_counts(l, w, b, "", show);
        }
    } else {
        for file in &files {
            match fs::read_to_string(file) {
                Ok(content) => {
                    let (l, w, b) = count(&content);
                    total.0 += l;
                    total.1 += w;
                    total.2 += b;
                    print_counts(l, w, b, file, show);
                }
                Err(e) => eprintln!("wc: {}: {}", file, e),
            }
        }
        if files.len() > 1 {
            print_counts(total.0, total.1, total.2, "total", show);
        }
    }
}
