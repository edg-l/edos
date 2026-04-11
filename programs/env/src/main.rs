use std::env;
use std::io::{self, Write};

fn main() {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (key, val) in env::vars() {
        let line = format!("{}={}\n", key, val);
        if out.write_all(line.as_bytes()).is_err() {
            break;
        }
    }
}
