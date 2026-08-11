//! A TCP echo server over the kernel socket syscalls.
//!
//! `std::net::TcpListener` is not implemented in the std fork, so this goes
//! through `edos_lib::net` directly: `bind`, `listen`, `accept`, then read and
//! write the accepted descriptor until the peer closes it. Connections are
//! served one at a time, in arrival order, which is what the kernel's accept
//! queue is for: a peer that connects while another is being echoed waits in
//! the backlog rather than being refused.

use std::env;
use std::process::exit;

use edos_lib::net::{self, SockAddrIn};

const DEFAULT_PORT: u16 = 7;

fn usage() -> ! {
    eprintln!("usage: tcpecho [-p port] [-a addr] [-1] [-q]");
    eprintln!("  -p port   port to listen on (default {DEFAULT_PORT})");
    eprintln!("  -a addr   address to bind (default 0.0.0.0)");
    eprintln!("  -1        serve a single connection, then exit");
    eprintln!("  -q        do not log connections");
    exit(2)
}

fn main() {
    let mut port = DEFAULT_PORT;
    let mut bind_ip = [0u8; 4];
    let mut once = false;
    let mut quiet = false;

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<u16>().ok()) {
                    Some(p) if p != 0 => port = p,
                    _ => usage(),
                }
            }
            "-a" => {
                i += 1;
                match args.get(i).and_then(|s| net::parse_ipv4(s)) {
                    Some(ip) => bind_ip = ip,
                    None => usage(),
                }
            }
            "-1" => once = true,
            "-q" => quiet = true,
            _ => usage(),
        }
        i += 1;
    }

    let Ok(listener) = net::create_tcp_socket() else {
        eprintln!("tcpecho: socket failed");
        exit(1);
    };

    let addr = SockAddrIn::new(bind_ip, port);
    if net::bind(listener, &addr).is_err() {
        eprintln!("tcpecho: bind to port {port} failed");
        net::close(listener);
        exit(1);
    }
    if net::listen(listener, 4).is_err() {
        eprintln!("tcpecho: listen failed");
        net::close(listener);
        exit(1);
    }
    if !quiet {
        println!(
            "listening on {}.{}.{}.{}:{}",
            bind_ip[0], bind_ip[1], bind_ip[2], bind_ip[3], port
        );
    }

    loop {
        let Ok((conn, peer)) = net::accept(listener) else {
            eprintln!("tcpecho: accept failed");
            net::close(listener);
            exit(1);
        };
        let peer_ip = peer.addr;
        let peer_port = u16::from_be(peer.port);
        if !quiet {
            println!(
                "accepted {}.{}.{}.{}:{}",
                peer_ip[0], peer_ip[1], peer_ip[2], peer_ip[3], peer_port
            );
        }

        let echoed = echo(conn);
        net::close(conn);
        if !quiet {
            match echoed {
                Ok(n) => println!("closed {peer_port} after {n} bytes"),
                Err(n) => println!("closed {peer_port} after {n} bytes (error)"),
            }
        }

        if once {
            break;
        }
    }

    net::close(listener);
}

/// Read and write back until the peer closes. Returns the byte count either way;
/// a short write is retried, since `send` is free to take less than it is given.
fn echo(conn: u64) -> Result<usize, usize> {
    let mut buf = [0u8; 2048];
    let mut total = 0usize;
    loop {
        let n = match net::recv(conn, &mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(_) => return Err(total),
        };
        if net::send_all(conn, &buf[..n]).is_err() {
            return Err(total);
        }
        total += n;
    }
}
