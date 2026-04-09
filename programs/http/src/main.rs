use std::env;
use std::io::{self, Write};

use edos_lib::net;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: http <ip> [path]");
        eprintln!("Example: http 93.184.216.34 /");
        return;
    }

    let ip = match parse_ip(&args[1]) {
        Some(ip) => ip,
        None => {
            eprintln!("Invalid IP: {}", args[1]);
            return;
        }
    };

    let path = args.get(2).map(|s| s.as_str()).unwrap_or("/");

    let fd = match net::create_tcp_socket() {
        Ok(fd) => fd,
        Err(_) => {
            eprintln!("Failed to create socket");
            return;
        }
    };

    let addr = net::SockAddrIn::new(ip, 80);
    if net::connect(fd, &addr).is_err() {
        eprintln!("Connection failed");
        net::close(fd);
        return;
    }

    // Send HTTP/1.0 request
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}.{}.{}.{}\r\n\r\n",
        path, ip[0], ip[1], ip[2], ip[3]
    );
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

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut ip = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        ip[i] = part.parse().ok()?;
    }
    Some(ip)
}
