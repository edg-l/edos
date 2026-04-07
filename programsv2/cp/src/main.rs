use std::env;
use std::fs;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: cp SOURCE DEST");
        std::process::exit(1);
    }

    let src = &args[1];
    let dst = &args[2];

    if let Err(e) = fs::copy(src, dst) {
        eprintln!("cp: {}: {}", src, e);
        std::process::exit(1);
    }
}
