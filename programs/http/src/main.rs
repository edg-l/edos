use std::env;
use std::io::{self, Read, Write};
use std::net::TcpStream;

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut include_headers = false;
    let mut verbose = false;
    let mut url = None;

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
            _ => url = Some(arg.as_str()),
        }
    }

    let url = match url {
        Some(u) => u,
        None => {
            eprintln!("http: missing URL (try http --help)");
            return;
        }
    };

    if url.starts_with("https://") {
        eprintln!("http: HTTPS is not supported (no TLS)");
        return;
    }

    let (host, port, path) = parse_url(url);

    let addr = format!("{}:{}", host, port);
    let mut stream = match TcpStream::connect(addr.as_str()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("http: connect to {}: {}", addr, e);
            return;
        }
    };

    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, host
    );

    if verbose {
        for line in request.lines() {
            eprintln!("> {}", line);
        }
    }

    if let Err(e) = stream.write_all(request.as_bytes()) {
        eprintln!("http: send: {}", e);
        return;
    }

    let mut response = Vec::new();
    if let Err(e) = stream.read_to_end(&mut response) {
        eprintln!("http: recv: {}", e);
        return;
    }

    // Split headers from body at \r\n\r\n
    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(response.len());

    let stdout = io::stdout();
    let mut out = stdout.lock();

    if include_headers {
        if verbose {
            // Print headers with "< " prefix to stderr
            if let Ok(headers) = core::str::from_utf8(&response[..header_end]) {
                for line in headers.lines() {
                    eprintln!("< {}", line);
                }
                eprintln!("<");
            }
        } else {
            let _ = out.write_all(&response[..header_end]);
            let _ = out.write_all(b"\r\n\r\n");
        }
    }

    let body_start = if header_end + 4 <= response.len() {
        header_end + 4
    } else {
        response.len()
    };
    let _ = out.write_all(&response[body_start..]);
    let _ = out.flush();
}

/// Parse a URL into (host, port, path).
/// Supports: "http://host:port/path", "host:port/path", "host/path", "host"
fn parse_url(url: &str) -> (&str, u16, &str) {
    let url = url.strip_prefix("http://").unwrap_or(url);

    // Split path from host:port
    let (hostport, path) = match url.find('/') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, "/"),
    };

    // Split port from host
    match hostport.rfind(':') {
        Some(i) => {
            if let Ok(port) = hostport[i + 1..].parse::<u16>() {
                (&hostport[..i], port, path)
            } else {
                (hostport, 80, path)
            }
        }
        None => (hostport, 80, path),
    }
}
