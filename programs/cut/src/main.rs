use edos_lib::args::{Opt, Spec};
use std::io::{self, BufRead, Write};

const SPEC: Spec = Spec::new(
    "cut",
    "-d DELIM -f LIST [file...]",
    &[
        Opt::arg(
            'd',
            "delimiter",
            "DELIM",
            "the field separator, one character",
        ),
        Opt::arg(
            'f',
            "fields",
            "LIST",
            "the fields to keep, comma separated, 1-based",
        ),
    ],
);

fn main() {
    let m = SPEC.parse_env();
    let Some(d) = m.value('d') else {
        SPEC.fail("-d is required");
    };
    let delim = d.chars().next().unwrap_or('\t');
    let mut fields: Vec<usize> = Vec::new();
    if let Some(list) = m.value('f') {
        for part in list.split(',') {
            if let Ok(n) = part.parse::<usize>()
                && n > 0
            {
                fields.push(n - 1);
            }
        }
    }
    if fields.is_empty() {
        SPEC.fail("-f is required and must name at least one field");
    }
    let files: Vec<&String> = m
        .positional()
        .iter()
        .filter(|p| p.as_str() != "-")
        .collect();

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
