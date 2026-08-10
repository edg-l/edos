use std::env;
use std::fs;
use std::io::{self, Read};

fn hexdump(data: &[u8]) {
    for (offset, chunk) in data.chunks(16).enumerate() {
        print!("{:08x}  ", offset * 16);

        // Hex bytes
        for (i, byte) in chunk.iter().enumerate() {
            if i == 8 {
                print!(" ");
            }
            print!("{:02x} ", byte);
        }
        // Pad if last chunk is short
        for i in chunk.len()..16 {
            if i == 8 {
                print!(" ");
            }
            print!("   ");
        }

        // ASCII
        print!(" |");
        for byte in chunk {
            if byte.is_ascii_graphic() || *byte == b' ' {
                print!("{}", *byte as char);
            } else {
                print!(".");
            }
        }
        println!("|");
    }
    if !data.is_empty() {
        println!("{:08x}", data.len());
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut limit: Option<usize> = None;
    let mut files: Vec<&str> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            files.extend(args[i + 1..].iter().map(String::as_str));
            break;
        } else if let Some(rest) = arg.strip_prefix("-n") {
            let value = if rest.is_empty() {
                i += 1;
                match args.get(i) {
                    Some(v) => v.as_str(),
                    None => {
                        eprintln!("hexdump: -n requires a length");
                        std::process::exit(1);
                    }
                }
            } else {
                rest
            };
            match value.parse::<usize>() {
                Ok(n) => limit = Some(n),
                Err(_) => {
                    eprintln!("hexdump: invalid length: {}", value);
                    std::process::exit(1);
                }
            }
        } else if arg.starts_with('-') && arg.len() > 1 {
            eprintln!("hexdump: unknown option {}", arg);
            std::process::exit(1);
        } else {
            files.push(arg);
        }
        i += 1;
    }

    // Read at most `limit` bytes rather than the whole file, so -n stays cheap
    // on a large image.
    let read_limited = |source: &mut dyn Read| -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        match limit {
            Some(n) => source.take(n as u64).read_to_end(&mut data)?,
            None => source.read_to_end(&mut data)?,
        };
        Ok(data)
    };

    if files.is_empty() {
        if let Ok(data) = read_limited(&mut io::stdin()) {
            hexdump(&data);
        }
    } else {
        for file in &files {
            let result = fs::File::open(file).and_then(|mut f| read_limited(&mut f));
            match result {
                Ok(data) => hexdump(&data),
                Err(e) => eprintln!("hexdump: {}: {}", file, e),
            }
        }
    }
}
