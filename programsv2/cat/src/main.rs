//! cat - concatenate files or stdin to stdout

use std::env;
use std::io::{self, Read, Write};

fn cat_stdin() {
    let mut buf = [0u8; 4096];
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        match stdin.lock().read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let _ = stdout.write_all(&buf[..n]);
                let _ = stdout.flush();
            }
            Err(_) => break,
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let files = &args[1..];

    if files.is_empty() {
        cat_stdin();
    } else {
        for path in files {
            if path == "-" {
                cat_stdin();
            } else {
                match std::fs::read(path) {
                    Ok(data) => {
                        let _ = io::stdout().write_all(&data);
                    }
                    Err(e) => {
                        eprintln!("cat: {}: {}", path, e);
                    }
                }
            }
        }
    }
}
