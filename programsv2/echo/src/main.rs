//! echo - print arguments to stdout

use std::env;

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
    let args = &args[1..]; // skip program name

    let output = if args.first().map(|s| s.as_str()) == Some("-e") {
        let text = args[1..].join(" ");
        format!("{}\n", expand_escapes(&text))
    } else {
        format!("{}\n", args.join(" "))
    };

    raw_write(1, output.as_bytes());
}

fn expand_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('e') => out.push('\x1B'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}
