use std::env;
use std::io::{self, Write};

fn main() {
    let args: Vec<String> = env::args().collect();
    let text = if args.len() > 1 {
        args[1..].join(" ")
    } else {
        "y".to_string()
    };
    let line = format!("{}\n", text);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    loop {
        if out.write_all(line.as_bytes()).is_err() {
            break;
        }
    }
}
