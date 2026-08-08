//! Minimal TCP client over the kernel socket syscalls.
//!
//! `std::net::TcpStream` is not implemented in the std fork, so this goes
//! through `edos_lib::net` directly. Exercises connect, send, recv and the
//! retransmit path against a real peer.

use std::env;

use edos_lib::net::{self, SockAddrIn};

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for slot in out.iter_mut() {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: tcptest <ipv4> <port> [path]");
        std::process::exit(2);
    }

    let Some(ip) = parse_ip(&args[1]) else {
        eprintln!("tcptest: bad address {}", args[1]);
        std::process::exit(2);
    };
    let Ok(port) = args[2].parse::<u16>() else {
        eprintln!("tcptest: bad port {}", args[2]);
        std::process::exit(2);
    };
    let path = args.get(3).map(|s| s.as_str()).unwrap_or("/");

    let fd = match net::create_tcp_socket() {
        Ok(fd) => fd,
        Err(_) => {
            eprintln!("tcptest: socket failed");
            std::process::exit(1);
        }
    };

    let addr = SockAddrIn::new(ip, port);
    if net::connect(fd, &addr).is_err() {
        eprintln!("tcptest: connect to {}.{}.{}.{}:{} failed", ip[0], ip[1], ip[2], ip[3], port);
        net::close(fd);
        std::process::exit(1);
    }
    println!("connected to {}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port);

    let request = format!("GET {} HTTP/1.0\r\nHost: {}.{}.{}.{}\r\n\r\n", path, ip[0], ip[1], ip[2], ip[3]);
    match net::send(fd, request.as_bytes()) {
        Ok(n) => println!("sent {} bytes", n),
        Err(_) => {
            eprintln!("tcptest: send failed");
            net::close(fd);
            std::process::exit(1);
        }
    }

    let mut total = 0usize;
    let mut first_line = String::new();
    let mut buf = [0u8; 2048];
    loop {
        match net::recv(fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if first_line.is_empty() {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    first_line = text.lines().next().unwrap_or("").to_string();
                }
                total += n;
                if total > 1 << 20 {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    net::close(fd);
    println!("received {} bytes, status line: {}", total, first_line);
    if total == 0 {
        eprintln!("tcptest: no data received");
        std::process::exit(1);
    }
    println!("tcptest: ok");
}
