//! ls - list directory contents

use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).map(|s| s.as_str()).unwrap_or(".");
    let color = edos_lib::io::isatty(1);

    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ls: cannot access '{}': {}", path, e);
            process::exit(1);
        }
    };

    let mut items: Vec<(String, bool)> = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) => {
                let name = e.file_name().to_string_lossy().into_owned();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                items.push((name, is_dir));
            }
            Err(e) => {
                eprintln!("ls: error reading entry: {}", e);
            }
        }
    }

    items.sort_by(|a, b| a.0.cmp(&b.0));
    items.dedup_by(|a, b| a.0 == b.0);

    if items.is_empty() {
        return;
    }

    let display_width =
        |item: &(String, bool)| -> usize { item.0.len() + if item.1 { 1 } else { 0 } };
    let max_name = items.iter().map(display_width).max().unwrap_or(0);
    let col_width = max_name + 2;
    let term_width = 80usize;
    let cols = (term_width / col_width).max(1);

    for (i, (name, is_dir)) in items.iter().enumerate() {
        if i > 0 && i % cols == 0 {
            println!();
        }
        let display = if *is_dir {
            format!("{}/", name)
        } else {
            name.clone()
        };
        let is_last_col = (i + 1) % cols == 0 || i + 1 == items.len();
        if *is_dir && color {
            if is_last_col {
                print!("\x1B[1;34m{}\x1B[0m", display);
            } else {
                print!("\x1B[1;34m{:<width$}\x1B[0m", display, width = col_width);
            }
        } else if is_last_col {
            print!("{}", display);
        } else {
            print!("{:<width$}", display, width = col_width);
        }
    }
    println!();
}
