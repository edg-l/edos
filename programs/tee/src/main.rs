use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut append = false;
    let mut files: Vec<&str> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-a" => append = true,
            arg if arg.starts_with('-') && arg.len() > 1 => {
                for c in arg[1..].chars() {
                    match c {
                        'a' => append = true,
                        _ => {
                            eprintln!("tee: unknown option -{}", c);
                            std::process::exit(1);
                        }
                    }
                }
            }
            _ => files.push(&args[i]),
        }
        i += 1;
    }

    let mut handles: Vec<Box<dyn Write>> = Vec::new();
    for path in &files {
        let f = match OpenOptions::new()
            .write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("tee: {}: {}", path, e);
                std::process::exit(1);
            }
        };
        handles.push(Box::new(f));
    }

    let mut buf = [0u8; 8192];
    let stdin = io::stdin();
    let mut inp = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    loop {
        let n = match inp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprintln!("tee: read: {}", e);
                std::process::exit(1);
            }
        };
        let chunk = &buf[..n];
        if out.write_all(chunk).is_err() {
            // stdout broken pipe -- still write to files
            for h in &mut handles {
                let _ = h.write_all(chunk);
            }
            break;
        }
        for h in &mut handles {
            if let Err(e) = h.write_all(chunk) {
                eprintln!("tee: write: {}", e);
            }
        }
    }
}
