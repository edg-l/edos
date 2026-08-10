//! ls - list directory contents

use std::env;
use std::fs;
use std::io::Write;
use std::process;

/// What a listed name is, which decides its suffix and its colour.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    File,
    Dir,
    Link,
}

impl Kind {
    /// The `ls -F` suffix: a directory ends in `/`, a symbolic link in `@`.
    fn suffix(self) -> &'static str {
        match self {
            Kind::File => "",
            Kind::Dir => "/",
            Kind::Link => "@",
        }
    }

    fn color(self) -> Option<&'static str> {
        match self {
            Kind::File => None,
            Kind::Dir => Some("\x1B[1;34m"),
            Kind::Link => Some("\x1B[1;36m"),
        }
    }
}

/// One listed name and what it is.
type Item = (String, Kind);

/// Classify a directory entry without following it, so a link to a directory
/// still reads as a link.
fn kind_of(ft: Option<std::fs::FileType>) -> Kind {
    match ft {
        Some(t) if t.is_symlink() => Kind::Link,
        Some(t) if t.is_dir() => Kind::Dir,
        _ => Kind::File,
    }
}

/// Whether `path` names a symbolic link. `readlink` is the only way to ask:
/// on this target `std`'s `lstat` follows links, so `symlink_metadata` reports
/// the target and `Metadata::is_symlink` is never true.
fn is_symlink(path: &str) -> bool {
    let mut buf = [0u8; 1];
    edos_lib::io::readlink(path, &mut buf) >= 0
}

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
                let kind = kind_of(e.file_type().ok());
                items.push((name, kind));
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
        for (name, kind) in items {
            println!("{}{}", name, kind.suffix());
        }
        return;
    }

    let display_width = |item: &Item| -> usize { item.0.len() + item.1.suffix().len() };
    let max_name = items.iter().map(display_width).max().unwrap_or(0);
    let col_width = max_name + 2;
    let term_width = 80usize;
    let cols = (term_width / col_width).max(1);

    for (i, (name, kind)) in items.iter().enumerate() {
        if i > 0 && i % cols == 0 {
            println!();
        }
        let display = format!("{}{}", name, kind.suffix());
        let is_last_col = (i + 1) % cols == 0 || i + 1 == items.len();
        let padded = if is_last_col {
            display
        } else {
            format!("{:<width$}", display, width = col_width)
        };
        match kind.color() {
            Some(color) => print!("{}{}\x1B[0m", color, padded),
            None => print!("{}", padded),
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
        // Named operands follow links, as POSIX requires: `ls a-link-to-a-dir`
        // lists the directory rather than printing the link's name.
        match fs::metadata(operand) {
            Ok(meta) if meta.is_dir() => dirs.push(operand.clone()),
            Ok(_) if is_symlink(operand) => files.push((operand.clone(), Kind::Link)),
            Ok(_) => files.push((operand.clone(), Kind::File)),
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
