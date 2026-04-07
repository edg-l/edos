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

    if args.len() < 2 {
        // Read from stdin
        let mut data = Vec::new();
        if io::stdin().read_to_end(&mut data).is_ok() {
            hexdump(&data);
        }
    } else {
        for file in &args[1..] {
            match fs::read(file) {
                Ok(data) => hexdump(&data),
                Err(e) => eprintln!("hexdump: {}: {}", file, e),
            }
        }
    }
}
