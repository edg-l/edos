use edos_lib::args::{Opt, Spec};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};

const SPEC: Spec = Spec::new(
    "tee",
    "[-a] [file...]",
    &[Opt::flag(
        'a',
        "append",
        "append to the files rather than truncating them",
    )],
);

fn main() {
    let m = SPEC.parse_env();
    let append = m.is_set('a');

    let mut handles: Vec<Box<dyn Write>> = Vec::new();
    for path in m.positional() {
        let f = match OpenOptions::new()
            .write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("tee: {}: {}", path, e);
                std::process::exit(1);
            }
        };
        handles.push(Box::new(f));
    }

    let mut buf = [0u8; 8192];
    let stdin = io::stdin();
    let mut inp = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    loop {
        let n = match inp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprintln!("tee: read: {}", e);
                std::process::exit(1);
            }
        };
        let chunk = &buf[..n];
        if out.write_all(chunk).is_err() {
            // stdout broken pipe -- still write to files
            for h in &mut handles {
                let _ = h.write_all(chunk);
            }
            break;
        }
        for h in &mut handles {
            if let Err(e) = h.write_all(chunk) {
                eprintln!("tee: write: {}", e);
            }
        }
    }
}
