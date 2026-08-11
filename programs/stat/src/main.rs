//! stat - show file metadata

use std::env;
use std::fs;
use std::process;

/// The target of `path` if it is a symbolic link, and `None` otherwise.
///
/// The link test and the target read are separate calls, so a path that stops
/// being a link between them reports no target rather than a wrong one.
fn link_target(path: &str) -> Option<String> {
    if !fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink()) {
        return None;
    }
    fs::read_link(path).ok()?.to_str().map(str::to_owned)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: stat <path>");
        process::exit(1);
    }

    let path = &args[1];

    // A link is described by itself: its own type, its own length, and where
    // it points. Whether the target resolves is a separate question.
    if let Some(target) = link_target(path) {
        println!("  File: \x1B[1m{}\x1B[0m", path);
        println!("  Type: \x1B[1;36msymbolic link\x1B[0m");
        println!("Target: \x1B[1;36m{}\x1B[0m", target);
        println!("  Size: \x1B[33m{}\x1B[0m bytes", target.len());
        return;
    }

    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("stat: {}: {}", path, e);
            process::exit(1);
        }
    };

    let (file_type, type_color) = if meta.is_dir() {
        ("directory", "\x1B[1;34m")
    } else {
        ("regular file", "\x1B[0m")
    };

    println!("  File: \x1B[1m{}\x1B[0m", path);
    println!("  Type: {}{}\x1B[0m", type_color, file_type);
    println!("  Size: \x1B[33m{}\x1B[0m bytes", meta.len());
}
