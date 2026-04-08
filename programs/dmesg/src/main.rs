//! dmesg - show kernel log

use std::fs::File;
use std::io::{self, Read, Write};
use std::process;

fn main() {
    let file = match File::open("/dev/klog") {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dmesg: failed to open /dev/klog: {}", e);
            process::exit(1);
        }
    };

    let mut buf = Vec::new();
    match file.take(128 * 1024).read_to_end(&mut buf) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("dmesg: failed to read /dev/klog: {}", e);
            process::exit(1);
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if let Err(e) = out.write_all(&buf) {
        eprintln!("dmesg: write error: {}", e);
        process::exit(1);
    }
}
