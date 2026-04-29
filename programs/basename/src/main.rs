use std::env;
use std::io::{self, Write};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: basename <path> [suffix]");
        std::process::exit(1);
    }

    let path = args[1].trim_end_matches('/');
    let name = path.rsplit('/').next().unwrap_or(path);
    let result = if args.len() > 2 {
        name.strip_suffix(args[2].as_str()).unwrap_or(name)
    } else {
        name
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(result.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}
