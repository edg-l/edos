//! `httpd` — serve a directory tree over HTTP.
//!
//! One thread per accepted connection, so a client that opens a socket and then
//! says nothing cannot stop the next one from being served. Each connection
//! answers one request and closes (`Connection: close`), which is the HTTP/1.0
//! shape every client still understands and needs no idle-timeout machinery.
//!
//! Sockets go through `edos_lib::net` rather than `std::net`, matching `nc` and
//! `tcpecho`: the listen and accept half is reachable there directly.
//!
//! `GET` and `HEAD` are served; anything else is 405. A directory is served as
//! its index file when one exists and as a generated listing otherwise.

use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::process::exit;
use std::sync::Arc;
use std::thread;

use edos_lib::net::{self, SockAddrIn};
use edos_lib::time::{ClockTime, clock_gettime};

/// Largest request head accepted, in bytes. A client that sends more than this
/// without a blank line is answered 400 rather than allowed to grow the buffer.
const MAX_REQUEST: usize = 8192;

/// Transfer chunk. Body bytes never all sit in memory, so a 5G file costs this.
const CHUNK: usize = 16384;

struct Config {
    root: String,
    index: String,
    verbose: bool,
    listing: bool,
}

fn usage() -> ! {
    eprintln!("usage: httpd [-p port] [-d dir] [-i index] [-b addr] [-vL]");
    eprintln!("  -p port   port to listen on (default 80)");
    eprintln!("  -d dir    directory to serve (default .)");
    eprintln!("  -i name   index file served for a directory (default index.html)");
    eprintln!("  -b addr   address to bind (default 0.0.0.0)");
    eprintln!("  -L        do not generate directory listings, answer 403");
    eprintln!("  -v        log every request on standard output");
    exit(2)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut port: u16 = 80;
    let mut bind_ip = [0u8; 4];
    let mut config = Config {
        root: String::from("."),
        index: String::from("index.html"),
        verbose: false,
        listing: true,
    };

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg.len() < 2 || !arg.starts_with('-') {
            usage();
        }
        let mut chars = arg[1..].chars();
        while let Some(c) = chars.next() {
            match c {
                'v' => config.verbose = true,
                'L' => config.listing = false,
                'p' | 'd' | 'i' | 'b' => {
                    if chars.next().is_some() {
                        usage();
                    }
                    i += 1;
                    let Some(value) = args.get(i) else { usage() };
                    match c {
                        'p' => match value.parse::<u16>() {
                            Ok(p) if p != 0 => port = p,
                            _ => usage(),
                        },
                        'd' => config.root = value.clone(),
                        'i' => config.index = value.clone(),
                        _ => match net::parse_ipv4(value) {
                            Some(ip) => bind_ip = ip,
                            None => usage(),
                        },
                    }
                }
                _ => usage(),
            }
        }
        i += 1;
    }

    match fs::metadata(&config.root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            eprintln!("httpd: {} is not a directory", config.root);
            exit(1);
        }
        Err(e) => {
            eprintln!("httpd: {}: {e}", config.root);
            exit(1);
        }
    }

    let Ok(listener) = net::create_tcp_socket() else {
        eprintln!("httpd: socket failed");
        exit(1);
    };
    let bound = SockAddrIn::new(bind_ip, port);
    if net::bind(listener, &bound).is_err() {
        eprintln!("httpd: bind to port {port} failed");
        net::close(listener);
        exit(1);
    }
    if net::listen(listener, 8).is_err() {
        eprintln!("httpd: listen failed");
        net::close(listener);
        exit(1);
    }
    println!(
        "httpd: serving {} on {}.{}.{}.{}:{}",
        config.root, bind_ip[0], bind_ip[1], bind_ip[2], bind_ip[3], port
    );

    let config = Arc::new(config);
    loop {
        let Ok((conn, peer)) = net::accept(listener) else {
            eprintln!("httpd: accept failed");
            net::close(listener);
            exit(1);
        };
        let config = Arc::clone(&config);
        // Detached: a connection outlives no state the listener owns, and a
        // join here would serialise exactly what the thread is for.
        thread::spawn(move || {
            serve(conn, peer, &config);
            net::close(conn);
        });
    }
}

/// Read one request from `sock`, answer it, and return.
fn serve(sock: u64, peer: SockAddrIn, config: &Config) {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() >= MAX_REQUEST {
            respond(sock, 400, "Bad Request", "text/plain", b"400 Bad Request\n");
            log(config, &peer, "-", "-", 400, 0);
            return;
        }
        match net::recv(sock, &mut buf) {
            Ok(0) | Err(_) => return, // client hung up before finishing a request
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }

    let text = String::from_utf8_lossy(&head).into_owned();
    let Some(request_line) = text.lines().next() else {
        respond(sock, 400, "Bad Request", "text/plain", b"400 Bad Request\n");
        log(config, &peer, "-", "-", 400, 0);
        return;
    };
    let mut fields = request_line.split_whitespace();
    let (Some(method), Some(target)) = (fields.next(), fields.next()) else {
        respond(sock, 400, "Bad Request", "text/plain", b"400 Bad Request\n");
        log(config, &peer, "-", "-", 400, 0);
        return;
    };

    if method != "GET" && method != "HEAD" {
        let sent = respond(
            sock,
            405,
            "Method Not Allowed",
            "text/plain",
            b"405 Method Not Allowed\n",
        );
        log(config, &peer, method, target, 405, sent);
        return;
    }

    // Everything from `?` on is the query string, and a client is free to send
    // an absolute URI instead of a path.
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let path = match path.find("://") {
        Some(i) => match path[i + 3..].find('/') {
            Some(j) => &path[i + 3 + j..],
            None => "/",
        },
        None => path,
    };

    let Some(rel) = safe_path(path) else {
        let sent = respond(sock, 403, "Forbidden", "text/plain", b"403 Forbidden\n");
        log(config, &peer, method, target, 403, sent);
        return;
    };

    let mut disk = config.root.clone();
    if !rel.is_empty() {
        if !disk.ends_with('/') {
            disk.push('/');
        }
        disk.push_str(&rel);
    }

    let Ok(meta) = fs::metadata(&disk) else {
        let sent = respond(sock, 404, "Not Found", "text/plain", b"404 Not Found\n");
        log(config, &peer, method, target, 404, sent);
        return;
    };

    if meta.is_dir() {
        // Without the trailing slash every relative link in the listing would
        // resolve one level too high, so redirect rather than serve.
        if !path.ends_with('/') {
            let sent = redirect(sock, &format!("{path}/"));
            log(config, &peer, method, target, 301, sent);
            return;
        }
        let index = format!("{}/{}", disk.trim_end_matches('/'), config.index);
        if fs::metadata(&index).map(|m| m.is_file()).unwrap_or(false) {
            let (status, sent) = send_file(sock, &index, method == "HEAD");
            log(config, &peer, method, target, status, sent);
            return;
        }
        if !config.listing {
            let sent = respond(sock, 403, "Forbidden", "text/plain", b"403 Forbidden\n");
            log(config, &peer, method, target, 403, sent);
            return;
        }
        let body = listing(&disk, path);
        let sent = if method == "HEAD" {
            head_only(sock, 200, "OK", "text/html; charset=utf-8", body.len())
        } else {
            respond(sock, 200, "OK", "text/html; charset=utf-8", body.as_bytes())
        };
        log(config, &peer, method, target, 200, sent);
        return;
    }

    let (status, sent) = send_file(sock, &disk, method == "HEAD");
    log(config, &peer, method, target, status, sent);
}

/// Turn a request path into a root-relative filesystem path, or reject it.
///
/// Rejection is the point: a `..` component, an absolute component or an
/// embedded NUL would otherwise reach outside the served tree. Percent escapes
/// are decoded first, so `%2e%2e` is caught as well.
fn safe_path(path: &str) -> Option<String> {
    let decoded = percent_decode(path);
    if decoded.contains('\0') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a path component for an `href`.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Generated index for a directory, sorted with subdirectories first.
fn listing(disk: &str, url: &str) -> String {
    let mut entries: Vec<(bool, String, u64)> = Vec::new();
    if let Ok(dir) = fs::read_dir(disk) {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (is_dir, size) = match entry.metadata() {
                Ok(m) => (m.is_dir(), m.len()),
                Err(_) => (false, 0),
            };
            entries.push((is_dir, name, size));
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let title = html_escape(url);
    let mut body = format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
         <title>Index of {title}</title></head>\n<body>\n<h1>Index of {title}</h1>\n<pre>\n"
    );
    if url != "/" {
        body.push_str("<a href=\"../\">../</a>\n");
    }
    for (is_dir, name, size) in entries {
        let href = url_encode(&name);
        let shown = html_escape(&name);
        if is_dir {
            body.push_str(&format!("<a href=\"{href}/\">{shown}/</a>\n"));
        } else {
            body.push_str(&format!("<a href=\"{href}\">{shown}</a>  {size}\n"));
        }
    }
    body.push_str("</pre>\n</body></html>\n");
    body
}

fn content_type(path: &str) -> &'static str {
    let ext = match path.rfind('.') {
        Some(i) => &path[i + 1..],
        None => "",
    };
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" | "md" | "log" | "rs" | "toml" | "sh" => "text/plain; charset=utf-8",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// `Date:` in the format RFC 9110 §5.6.7 requires, which is always GMT.
fn http_date() -> String {
    let Some(now) = clock_gettime() else {
        return String::from("Thu, 01 Jan 1970 00:00:00 GMT");
    };
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let ClockTime {
        hour,
        minute,
        second,
        day,
        month,
        year,
        weekday,
    } = now;
    format!(
        "{}, {day:02} {} {year} {hour:02}:{minute:02}:{second:02} GMT",
        DAYS[(weekday as usize) % 7],
        MONTHS[(month as usize).clamp(1, 12) - 1]
    )
}

fn header(status: u16, reason: &str, ctype: &str, len: usize) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Date: {}\r\n\
         Server: edos-httpd\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        http_date()
    )
}

/// Send a complete small response. Returns the body bytes sent.
fn respond(sock: u64, status: u16, reason: &str, ctype: &str, body: &[u8]) -> u64 {
    let mut out = header(status, reason, ctype, body.len()).into_bytes();
    out.extend_from_slice(body);
    if net::send_all(sock, &out).is_err() {
        return 0;
    }
    body.len() as u64
}

/// Send only the head of a response, for `HEAD`.
fn head_only(sock: u64, status: u16, reason: &str, ctype: &str, len: usize) -> u64 {
    let _ = net::send_all(sock, header(status, reason, ctype, len).as_bytes());
    0
}

fn redirect(sock: u64, location: &str) -> u64 {
    let body = format!("301 Moved to {location}\n");
    let response = format!(
        "HTTP/1.1 301 Moved Permanently\r\n\
         Date: {}\r\n\
         Server: edos-httpd\r\n\
         Location: {location}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        http_date(),
        body.len()
    );
    let _ = net::send_all(sock, response.as_bytes());
    body.len() as u64
}

/// Stream a regular file, returning the status sent and the body bytes with it.
/// The body goes out in `CHUNK` pieces, so the served file never has to fit in
/// memory alongside itself.
fn send_file(sock: u64, disk: &str, head_only_request: bool) -> (u16, u64) {
    let (Ok(meta), Ok(mut file)) = (fs::metadata(disk), File::open(disk)) else {
        return (
            404,
            respond(sock, 404, "Not Found", "text/plain", b"404 Not Found\n"),
        );
    };
    let len = meta.len() as usize;
    let ctype = content_type(disk);
    if net::send_all(sock, header(200, "OK", ctype, len).as_bytes()).is_err() {
        return (200, 0);
    }
    if head_only_request {
        return (200, 0);
    }

    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if net::send_all(sock, &buf[..n]).is_err() {
                    break;
                }
                sent += n as u64;
            }
            Err(_) => break,
        }
    }
    (200, sent)
}

fn log(config: &Config, peer: &SockAddrIn, method: &str, target: &str, status: u16, bytes: u64) {
    if !config.verbose {
        return;
    }
    let ip = peer.addr;
    println!(
        "{}.{}.{}.{}:{} {method} {target} {status} {bytes}",
        ip[0],
        ip[1],
        ip[2],
        ip[3],
        u16::from_be(peer.port)
    );
}
