use std::env;
use std::io::{self, Write};

use edos_lib::net;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: http <host> [port] [path]");
        eprintln!("Example: http edgl.dev 80 /");
        return;
    }

    let ip = match resolve_host(&args[1]) {
        Some(ip) => ip,
        None => {
            eprintln!("Invalid IP: {}", args[1]);
            return;
        }
    };

    // Optional port (default 80) and path (default /)
    let (port, path) = if args.len() >= 3 {
        if let Ok(p) = args[2].parse::<u16>() {
            (p, args.get(3).map(|s| s.as_str()).unwrap_or("/"))
        } else {
            (80, args[2].as_str())
        }
    } else {
        (80, "/")
    };

    let fd = match net::create_tcp_socket() {
        Ok(fd) => fd,
        Err(_) => {
            eprintln!("Failed to create socket");
            return;
        }
    };

    let addr = net::SockAddrIn::new(ip, port);
    if net::connect(fd, &addr).is_err() {
        eprintln!("Connection failed");
        net::close(fd);
        return;
    }

    // Send HTTP/1.0 request
    let host = &args[1];
    let request = format!("GET {} HTTP/1.0\r\nHost: {}\r\n\r\n", path, host);
    if net::send(fd, request.as_bytes()).is_err() {
        eprintln!("Send failed");
        net::close(fd);
        return;
    }

    // Read response until connection closes
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = [0u8; 4096];
    loop {
        match net::recv(fd, &mut buf) {
            Ok(0) => break, // Connection closed
            Ok(n) => {
                let _ = out.write_all(&buf[..n]);
            }
            Err(_) => break,
        }
    }
    let _ = out.flush();

    net::close(fd);
}

fn resolve_host(s: &str) -> Option<[u8; 4]> {
    if s == "localhost" {
        return Some([127, 0, 0, 1]);
    }
    // Try dotted-quad IP first
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() == 4 {
        let mut ip = [0u8; 4];
        let mut ok = true;
        for (i, part) in parts.iter().enumerate() {
            match part.parse() {
                Ok(v) => ip[i] = v,
                Err(_) => { ok = false; break; }
            }
        }
        if ok {
            return Some(ip);
        }
    }
    // Fall back to DNS
    net::dns_resolve(s)
}
