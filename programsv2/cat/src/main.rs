//! cat - concatenate files or stdin to stdout

use std::env;
use std::io::{self, Write};

fn raw_read(fd: u64, buf: &mut [u8]) -> isize {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 0u64, // SYS_READ
            in("rdi") fd,
            in("rsi") buf.as_mut_ptr(),
            in("rdx") buf.len(),
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result as isize
}

fn raw_write(fd: u64, buf: &[u8]) -> isize {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 1u64, // SYS_WRITE
            in("rdi") fd,
            in("rsi") buf.as_ptr(),
            in("rdx") buf.len(),
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result as isize
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let files = &args[1..];

    if files.is_empty() {
        // No args: read from stdin and write to stdout using raw syscalls
        let mut buf = [0u8; 4096];
        loop {
            let n = raw_read(0, &mut buf);
            if n <= 0 {
                break;
            }
            raw_write(1, &buf[..n as usize]);
        }
    } else {
        // Read each file, then also read stdin if "-" is an argument
        for path in files {
            if path == "-" {
                let mut buf = [0u8; 4096];
                loop {
                    let n = raw_read(0, &mut buf);
                    if n <= 0 {
                        break;
                    }
                    raw_write(1, &buf[..n as usize]);
                }
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
