use std::env;
use std::io::{self, Write};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: dirname <path>");
        std::process::exit(1);
    }

    let path = args[1].trim_end_matches('/');
    let dir = match path.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent,
        Some((_, _)) => "/",
        None => ".",
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(dir.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}
