use std::env;
use std::io::{self, BufRead, Write};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: cut -d<delim> -f<fields,...> [file...]");
        std::process::exit(1);
    }

    let mut delim = '\t';
    let mut delim_set = false;
    let mut fields: Vec<usize> = Vec::new();

    let mut files: Vec<String> = Vec::new();
    for arg in &args[1..] {
        if let Some(d) = arg.strip_prefix("-d") {
            delim = d.chars().next().unwrap_or('\t');
            delim_set = true;
        } else if let Some(f) = arg.strip_prefix("-f") {
            for part in f.split(',') {
                if let Ok(n) = part.parse::<usize>() {
                    if n > 0 {
                        fields.push(n - 1);
                    }
                }
            }
        } else {
            files.push(arg.clone());
        }
    }

    if fields.is_empty() || !delim_set {
        eprintln!("cut: -f and -d are required");
        std::process::exit(1);
    }

    fields.sort();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if files.is_empty() {
        process_reader(io::stdin().lock(), delim, &fields, &mut out);
    } else {
        for path in &files {
            match std::fs::File::open(path) {
                Ok(file) => {
                    process_reader(io::BufReader::new(file), delim, &fields, &mut out);
                }
                Err(e) => {
                    eprintln!("cut: {}: {}", path, e);
                    std::process::exit(1);
                }
            }
        }
    }
}

fn process_reader(reader: impl BufRead, delim: char, fields: &[usize], out: &mut impl Write) {
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            let _ = out.write_all(b"\n");
            continue;
        }
        let parts: Vec<&str> = line.split(delim).collect();
        let mut first = true;
        for &f in fields {
            if f >= parts.len() {
                continue;
            }
            if !first {
                let _ = out.write_all(&[delim as u8]);
            }
            first = false;
            let _ = out.write_all(parts[f].as_bytes());
        }
        let _ = out.write_all(b"\n");
    }
}
