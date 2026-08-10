//! ls - list directory contents

use std::env;
use std::fs;
use std::io::Write;
use std::process;

/// One listed name and whether it is a directory.
type Item = (String, bool);

/// Read a directory into sorted items.
fn read_dir_items(path: &str) -> Option<Vec<Item>> {
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ls: cannot access '{}': {}", path, e);
            return None;
        }
    };

    let mut items: Vec<Item> = Vec::new();
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
    Some(items)
}

/// Print items in columns on a terminal, one per line otherwise.
fn print_items(items: &[Item], is_tty: bool) {
    if items.is_empty() {
        return;
    }

    if !is_tty {
        // Piped output: one entry per line, no color, no padding
        for (name, is_dir) in items {
            if *is_dir {
                println!("{}/", name);
            } else {
                println!("{}", name);
            }
        }
        return;
    }

    let display_width = |item: &Item| -> usize { item.0.len() + if item.1 { 1 } else { 0 } };
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
        if *is_dir {
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

fn main() {
    let mut operands: Vec<String> = env::args().skip(1).collect();
    if operands.is_empty() {
        operands.push(".".to_string());
    }
    let is_tty = edos_lib::io::isatty(1);
    let mut status = 0;

    // Non-directory operands are listed by name first, then each directory's
    // contents, which is what makes `ls *.txt` and `ls a-file a-dir` work.
    let mut files: Vec<Item> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for operand in &operands {
        match fs::metadata(operand) {
            Ok(meta) if meta.is_dir() => dirs.push(operand.clone()),
            Ok(_) => files.push((operand.clone(), false)),
            Err(e) => {
                eprintln!("ls: cannot access '{}': {}", operand, e);
                status = 1;
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    dirs.sort();

    print_items(&files, is_tty);

    // A single directory operand is listed bare; several operands each get a
    // header so the listings can be told apart.
    let headers = operands.len() > 1;
    for (i, dir) in dirs.iter().enumerate() {
        if headers {
            if i > 0 || !files.is_empty() {
                println!();
            }
            println!("{}:", dir);
        }
        match read_dir_items(dir) {
            Some(items) => print_items(&items, is_tty),
            None => status = 1,
        }
    }

    let _ = std::io::stdout().flush();
    process::exit(status);
}
