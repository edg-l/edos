//! Fetch a URL and print it.

use std::{
    env,
    io::{self, Write},
    process,
};

use edos_http::{Options, get};

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut include_headers = false;
    let mut verbose = false;
    let mut url_arg = None;

    for arg in &args[1..] {
        match arg.as_str() {
            "-i" => include_headers = true,
            "-v" => {
                verbose = true;
                include_headers = true;
            }
            "-h" | "--help" => {
                usage();
                return;
            }
            _ => url_arg = Some(arg.as_str()),
        }
    }

    let Some(url_arg) = url_arg else {
        eprintln!("http: missing URL (try http --help)");
        process::exit(1);
    };

    let response = match get(url_arg, &Options::default()) {
        Ok(response) => response,
        Err(e) => {
            eprintln!("http: {}: {}", url_arg, e);
            process::exit(1);
        }
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    if verbose {
        for line in response.head.sent.lines() {
            eprintln!("> {}", line);
        }
        for line in response.head.raw_headers.lines() {
            eprintln!("< {}", line);
        }
        eprintln!("<");
    } else if include_headers {
        let _ = out.write_all(response.head.raw_headers.as_bytes());
    }

    let _ = out.write_all(&response.body);
    let _ = out.flush();

    if !response.head.is_success() {
        process::exit(1);
    }
}

fn usage() {
    eprintln!("usage: http [-i] [-v] URL");
    eprintln!("  -i  include the response headers");
    eprintln!("  -v  show the request and response headers on stderr");
    eprintln!("Examples:");
    eprintln!("  http https://edos.edgl.dev/pkg/index");
    eprintln!("  http edgl.dev");
    eprintln!("  http 10.0.2.2:8000/path");
}
