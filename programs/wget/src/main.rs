//! Download a URL to a file.

use std::{
    env,
    fs::File,
    io::{self, Write},
    process,
};

use edos_http::{Options, fetch, url::Url};

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut output: Option<String> = None;
    let mut quiet = false;
    let mut url_arg: Option<String> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-O" => {
                i += 1;
                if i >= args.len() {
                    fail("-O requires an argument");
                }
                output = Some(args[i].clone());
            }
            "-q" | "--quiet" => quiet = true,
            "-h" | "--help" => {
                usage();
                process::exit(0);
            }
            arg if arg.starts_with("-O") => output = Some(args[i][2..].to_string()),
            arg if arg.starts_with('-') && arg.len() > 1 => {
                fail(&format!("unknown option: {}", arg));
            }
            _ => url_arg = Some(args[i].clone()),
        }
        i += 1;
    }

    let Some(url_arg) = url_arg else {
        usage();
        process::exit(1);
    };

    let dest = output.unwrap_or_else(|| match Url::parse(&url_arg) {
        Ok(url) => url.filename().to_string(),
        Err(_) => "index.html".to_string(),
    });

    let opts = Options::default();
    let mut reporter = Reporter::new(quiet);

    // Writing to a file as the body arrives rather than buffering it: a package
    // can be larger than the room this machine has to hold it twice.
    let result = if dest == "-" {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        fetch(&url_arg, &opts, &mut out, &mut |done, total| {
            reporter.update(done, total)
        })
    } else {
        match File::create(&dest) {
            Ok(mut file) => fetch(&url_arg, &opts, &mut file, &mut |done, total| {
                reporter.update(done, total)
            }),
            Err(e) => {
                eprintln!("wget: {}: {}", dest, e);
                process::exit(1);
            }
        }
    };

    let head = match result {
        Ok(head) => head,
        Err(e) => {
            reporter.clear();
            eprintln!("wget: {}: {}", url_arg, e);
            process::exit(1);
        }
    };

    reporter.clear();

    if !head.is_success() {
        eprintln!("wget: {}: {} {}", url_arg, head.status, head.reason);
        process::exit(1);
    }
    if !quiet {
        eprintln!(
            "wget: saved to '{}' ({} bytes)",
            dest,
            reporter.written_total()
        );
    }
}

/// A one-line progress report on stderr, rewritten in place.
struct Reporter {
    quiet: bool,
    written: u64,
    shown: bool,
}

impl Reporter {
    fn new(quiet: bool) -> Self {
        Reporter {
            quiet,
            written: 0,
            shown: false,
        }
    }

    fn update(&mut self, done: u64, total: Option<u64>) {
        self.written = done;
        if self.quiet {
            return;
        }
        match total {
            Some(total) if total > 0 => {
                let percent = done.saturating_mul(100) / total;
                eprint!("\r{} / {} bytes ({}%)   ", done, total, percent);
            }
            _ => eprint!("\r{} bytes   ", done),
        }
        let _ = io::stderr().flush();
        self.shown = true;
    }

    /// Take the progress line back off the terminal, so an error or the final
    /// message is not printed onto the end of it.
    fn clear(&mut self) {
        if self.shown {
            eprint!("\r{:60}\r", "");
            let _ = io::stderr().flush();
            self.shown = false;
        }
    }

    fn written_total(&self) -> u64 {
        self.written
    }
}

fn fail(message: &str) -> ! {
    eprintln!("wget: {}", message);
    usage();
    process::exit(2);
}

fn usage() {
    eprintln!("usage: wget [-q] [-O OUTPUT] URL");
    eprintln!("  -O  write to OUTPUT, or to stdout when it is '-'");
    eprintln!("  -q  no progress reporting");
    eprintln!("http:// and https:// are both supported.");
}
