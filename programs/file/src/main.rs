//! file - identify a file's type by its leading bytes

use std::env;
use std::fs;
use std::io::Read;
use std::process::ExitCode;

/// (magic, offset, description). Longest magic wins, so more specific
/// signatures can shadow shorter prefixes.
const MAGIC: &[(&[u8], usize, &str)] = &[
    (b"\x7fELF", 0, "ELF executable"),
    (b"\x89PNG\r\n\x1a\n", 0, "PNG image data"),
    (b"\xff\xd8\xff", 0, "JPEG image data"),
    (b"GIF87a", 0, "GIF image data, version 87a"),
    (b"GIF89a", 0, "GIF image data, version 89a"),
    (b"BM", 0, "PC bitmap"),
    (b"RIFF", 0, "RIFF container"),
    (b"WAVE", 8, "WAVE audio"),
    (b"OggS", 0, "Ogg data"),
    (b"fLaC", 0, "FLAC audio"),
    (b"\x1f\x8b", 0, "gzip compressed data"),
    (b"BZh", 0, "bzip2 compressed data"),
    (b"\xfd7zXZ\x00", 0, "XZ compressed data"),
    (b"PK\x03\x04", 0, "Zip archive data"),
    (b"ustar", 257, "POSIX tar archive"),
    (b"!<arch>", 0, "current ar archive"),
    (b"\xca\xfe\xba\xbe", 0, "Java class data"),
    (b"%PDF-", 0, "PDF document"),
    (b"#!", 0, "script text executable"),
    (b"EFS!", 0, "EDOS EFS filesystem image"),
];

fn describe_elf(head: &[u8]) -> String {
    let mut s = String::from("ELF");
    match head.get(4) {
        Some(1) => s.push_str(" 32-bit"),
        Some(2) => s.push_str(" 64-bit"),
        _ => {}
    }
    match head.get(5) {
        Some(1) => s.push_str(" LSB"),
        Some(2) => s.push_str(" MSB"),
        _ => {}
    }
    match head.get(16) {
        Some(1) => s.push_str(" relocatable"),
        Some(2) => s.push_str(" executable"),
        Some(3) => s.push_str(" shared object"),
        Some(4) => s.push_str(" core file"),
        _ => s.push_str(" object"),
    }
    if head.get(18) == Some(&0x3e) {
        s.push_str(", x86-64");
    }
    s
}

/// Text if every byte is printable ASCII, whitespace, or valid UTF-8.
fn looks_like_text(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    if data.contains(&0) {
        return false;
    }
    match core::str::from_utf8(data) {
        Ok(_) => true,
        // A multi-byte sequence can legitimately straddle the read boundary.
        Err(e) => e.valid_up_to() + 4 >= data.len(),
    }
}

fn identify(data: &[u8]) -> String {
    let mut best: Option<(&[u8], &str)> = None;
    for (magic, offset, desc) in MAGIC {
        if data.len() >= offset + magic.len() && &data[*offset..offset + magic.len()] == *magic
            && best.is_none_or(|(m, _)| magic.len() > m.len()) {
                best = Some((magic, desc));
            }
    }

    if let Some((magic, desc)) = best {
        if magic == b"\x7fELF" {
            return describe_elf(data);
        }
        return desc.to_string();
    }

    if data.is_empty() {
        return "empty".to_string();
    }
    if looks_like_text(data) {
        return "ASCII text".to_string();
    }
    "data".to_string()
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: file FILE...");
        println!("Identify each FILE by its leading bytes.");
        return if args.is_empty() {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }

    let mut failed = false;
    for path in &args {
        match fs::metadata(path) {
            Ok(meta) if meta.is_dir() => {
                println!("{path}: directory");
                continue;
            }
            Err(e) => {
                println!("{path}: cannot open ({e})");
                failed = true;
                continue;
            }
            _ => {}
        }

        let mut head = [0u8; 512];
        match fs::File::open(path).and_then(|mut f| f.read(&mut head)) {
            Ok(n) => println!("{path}: {}", identify(&head[..n])),
            Err(e) => {
                println!("{path}: cannot read ({e})");
                failed = true;
            }
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
