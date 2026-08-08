use std::env;
use std::io::{self, Write};

use edos_lib::http::{self, Url};

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
                eprintln!("Usage: http [options] <url>");
                eprintln!("  -i  Include response headers");
                eprintln!("  -v  Verbose (show request + response headers)");
                eprintln!("Examples:");
                eprintln!("  http http://edgl.dev/");
                eprintln!("  http edgl.dev");
                eprintln!("  http 10.0.2.2:8000/path");
                return;
            }
            _ => url_arg = Some(arg.as_str()),
        }
    }

    let Some(url_arg) = url_arg else {
        eprintln!("http: missing URL (try http --help)");
        return;
    };

    let url = match Url::parse(url_arg) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("http: {}: {}", url_arg, e);
            return;
        }
    };

    if verbose {
        for line in http::request_text(&url).lines() {
            eprintln!("> {}", line);
        }
    }

    let response = match http::get(&url) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("http: {}:{}: {}", url.host, url.port, e);
            return;
        }
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    if include_headers {
        if verbose {
            if let Ok(headers) = std::str::from_utf8(&response.head) {
                for line in headers.lines() {
                    eprintln!("< {}", line);
                }
                eprintln!("<");
            }
        } else {
            let _ = out.write_all(&response.head);
            let _ = out.write_all(b"\r\n\r\n");
        }
    }

    let _ = out.write_all(&response.body);
    let _ = out.flush();
}
