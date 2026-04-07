//! ls - list directory contents

use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).map(|s| s.as_str()).unwrap_or(".");

    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ls: {}: {}", path, e);
            process::exit(1);
        }
    };

    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) => {
                let mut name = e.file_name().to_string_lossy().into_owned();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    name.push('/');
                }
                names.push(name);
            }
            Err(e) => {
                eprintln!("ls: error reading entry: {}", e);
            }
        }
    }

    names.sort();

    // Print in columns separated by two spaces
    if names.is_empty() {
        return;
    }

    let col_width = names.iter().map(|n| n.len()).max().unwrap_or(0) + 2;
    let term_width = 80usize;
    let cols = (term_width / col_width).max(1);

    for (i, name) in names.iter().enumerate() {
        if i > 0 && i % cols == 0 {
            println!();
        }
        print!("{:<width$}", name, width = col_width);
    }
    println!();
}
