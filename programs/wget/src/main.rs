use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;

fn parse_url(url: &str) -> (&str, u16, &str) {
    let url = url.strip_prefix("http://").unwrap_or(url);

    let (hostport, path) = match url.find('/') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, "/"),
    };

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

fn filename_from_path(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) if i + 1 < trimmed.len() => &trimmed[i + 1..],
        _ => "index.html",
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut output: Option<&str> = None;
    let mut url: Option<&str> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-O" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("wget: -O requires an argument");
                    std::process::exit(1);
                }
                output = Some(&args[i]);
            }
            arg if arg.starts_with("-O") => {
                output = Some(&args[i][2..]);
            }
            _ => url = Some(&args[i]),
        }
        i += 1;
    }

    let url = match url {
        Some(u) => u,
        None => {
            eprintln!("usage: wget [-O output] <url>");
            std::process::exit(1);
        }
    };

    if url.starts_with("https://") {
        eprintln!("wget: HTTPS is not supported (no TLS)");
        std::process::exit(1);
    }

    let (host, port, path) = parse_url(url);

    let addr = format!("{}:{}", host, port);
    let mut stream = match TcpStream::connect(addr.as_str()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wget: connect to {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, host
    );

    if let Err(e) = stream.write_all(request.as_bytes()) {
        eprintln!("wget: send: {}", e);
        std::process::exit(1);
    }

    let mut response = Vec::new();
    if let Err(e) = stream.read_to_end(&mut response) {
        eprintln!("wget: recv: {}", e);
        std::process::exit(1);
    }

    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(response.len());

    let body_start = if header_end + 4 <= response.len() {
        header_end + 4
    } else {
        response.len()
    };
    let body = &response[body_start..];

    let dest = output.unwrap_or_else(|| filename_from_path(path));

    if dest == "-" {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = out.write_all(body);
    } else {
        if let Err(e) = fs::write(dest, body) {
            eprintln!("wget: {}: {}", dest, e);
            std::process::exit(1);
        }
        eprintln!("wget: saved to '{}'", dest);
    }
}
