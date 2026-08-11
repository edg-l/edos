//! `nc` — read and write a TCP connection, in either direction.
//!
//! Two modes over one relay loop. `nc host port` connects; `nc -l port` binds,
//! listens and accepts. Either way the loop polls standard input and the socket
//! together and copies whichever is ready, so a pipe on one side and a peer on
//! the other never block each other.
//!
//! `std::net` is not implemented in the std fork, so this goes through
//! `edos_lib::net` directly. Standard input is read with the raw `read` syscall
//! rather than through `std::io::Stdin`: `poll` reports what the descriptor
//! holds, and a buffered reader that had already drained it would make the loop
//! wait on data it has in hand.
//!
//! End of input on standard input shuts down the write half (`SHUT_WR`) instead
//! of closing the socket, so `echo hi | nc host 7` sends its line, lets the peer
//! see end of input, and still receives the answer.

use std::env;
use std::io::{self, Write};
use std::process::exit;

use edos_lib::io::{PollState, SelectFd, poll, sys_read};
use edos_lib::net::{self, SockAddrIn};

/// Wait forever, the `poll` encoding for "no timeout".
const FOREVER: u64 = u64::MAX;

fn usage() -> ! {
    eprintln!("usage: nc [-lnvkz] [-w secs] [-q secs] [-p port] [-s addr] [host] [port]");
    eprintln!("  -l        listen for an inbound connection instead of making one");
    eprintln!("  -p port   port to listen on (with -l), instead of the operand");
    eprintln!("  -s addr   address to bind (with -l, default 0.0.0.0)");
    eprintln!("  -k        keep listening after a connection closes");
    eprintln!("  -n        numeric addresses only, never resolve a name");
    eprintln!("  -z        connect and report, transfer nothing");
    eprintln!("  -w secs   quit after secs with no traffic in either direction");
    eprintln!("  -q secs   quit secs after end of input (default: wait for the peer)");
    eprintln!("  -v        report connections on stderr");
    exit(2)
}

struct Options {
    listen: bool,
    keep: bool,
    numeric: bool,
    scan: bool,
    verbose: bool,
    idle_ms: u64,
    quit_ms: u64,
    bind_ip: [u8; 4],
    port: Option<u16>,
    host: Option<String>,
}

fn parse_secs(arg: Option<&String>) -> u64 {
    match arg.and_then(|s| s.parse::<u64>().ok()) {
        Some(secs) if secs <= u64::MAX / 1000 => secs * 1000,
        _ => usage(),
    }
}

fn parse_args() -> Options {
    let mut opts = Options {
        listen: false,
        keep: false,
        numeric: false,
        scan: false,
        verbose: false,
        idle_ms: FOREVER,
        quit_ms: FOREVER,
        bind_ip: [0; 4],
        port: None,
        host: None,
    };

    let args: Vec<String> = env::args().skip(1).collect();
    let mut operands: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg.len() > 1 && arg.starts_with('-') {
            // Clustered flags, with the argument-taking ones ending a cluster.
            let mut chars = arg[1..].chars();
            while let Some(c) = chars.next() {
                match c {
                    'l' => opts.listen = true,
                    'k' => opts.keep = true,
                    'n' => opts.numeric = true,
                    'z' => opts.scan = true,
                    'v' => opts.verbose = true,
                    'w' | 'q' | 'p' | 's' => {
                        if chars.next().is_some() {
                            usage();
                        }
                        i += 1;
                        let value = args.get(i);
                        match c {
                            'w' => opts.idle_ms = parse_secs(value),
                            'q' => opts.quit_ms = parse_secs(value),
                            'p' => match value.and_then(|s| s.parse::<u16>().ok()) {
                                Some(p) if p != 0 => opts.port = Some(p),
                                _ => usage(),
                            },
                            _ => match value.and_then(|s| net::parse_ipv4(s)) {
                                Some(ip) => opts.bind_ip = ip,
                                None => usage(),
                            },
                        }
                    }
                    _ => usage(),
                }
            }
        } else {
            operands.push(arg);
        }
        i += 1;
    }

    // A listener takes just a port; a client takes host and port. `-p` supplies
    // the listening port on its own, which is how the older netcat spelt it.
    match operands.as_slice() {
        [] => {}
        [port] if opts.listen => opts.port = Some(parse_port(port)),
        [host, port] => {
            opts.host = Some((*host).clone());
            opts.port = Some(parse_port(port));
        }
        _ => usage(),
    }
    opts
}

fn parse_port(s: &str) -> u16 {
    match s.parse::<u16>() {
        Ok(p) if p != 0 => p,
        _ => {
            eprintln!("nc: bad port '{s}'");
            exit(2)
        }
    }
}

fn show(addr: &SockAddrIn) -> String {
    let ip = addr.addr;
    format!(
        "{}.{}.{}.{}:{}",
        ip[0],
        ip[1],
        ip[2],
        ip[3],
        u16::from_be(addr.port)
    )
}

fn main() {
    let opts = parse_args();
    let Some(port) = opts.port else { usage() };

    if opts.listen {
        listen_mode(&opts, port);
    } else {
        connect_mode(&opts, port);
    }
}

fn connect_mode(opts: &Options, port: u16) {
    let Some(host) = opts.host.as_deref() else {
        usage()
    };
    let ip = match net::parse_ipv4(host) {
        Some(ip) => ip,
        None if opts.numeric => {
            eprintln!("nc: '{host}' is not an address and -n forbids resolving it");
            exit(1)
        }
        None => match net::resolve_host(host) {
            Some(ip) => ip,
            None => {
                eprintln!("nc: cannot resolve {host}");
                exit(1)
            }
        },
    };

    let addr = SockAddrIn::new(ip, port);
    let Ok(sock) = net::create_tcp_socket() else {
        eprintln!("nc: socket failed");
        exit(1);
    };
    if net::connect(sock, &addr).is_err() {
        eprintln!("nc: connect to {} failed", show(&addr));
        net::close(sock);
        exit(1);
    }
    if opts.verbose || opts.scan {
        eprintln!("connected to {}", show(&addr));
    }

    if opts.scan {
        net::close(sock);
        return;
    }

    let outcome = relay(sock, opts);
    net::close(sock);
    exit(outcome);
}

fn listen_mode(opts: &Options, port: u16) {
    let Ok(listener) = net::create_tcp_socket() else {
        eprintln!("nc: socket failed");
        exit(1);
    };
    let bound = SockAddrIn::new(opts.bind_ip, port);
    if net::bind(listener, &bound).is_err() {
        eprintln!("nc: bind to {} failed", show(&bound));
        net::close(listener);
        exit(1);
    }
    if net::listen(listener, 4).is_err() {
        eprintln!("nc: listen failed");
        net::close(listener);
        exit(1);
    }
    if opts.verbose {
        eprintln!("listening on {}", show(&bound));
    }

    loop {
        let Ok((conn, peer)) = net::accept(listener) else {
            eprintln!("nc: accept failed");
            net::close(listener);
            exit(1);
        };
        if opts.verbose {
            eprintln!("connection from {}", show(&peer));
        }

        let outcome = relay(conn, opts);
        net::close(conn);
        if !opts.keep {
            net::close(listener);
            exit(outcome);
        }
    }
}

/// Copy between standard input and `sock` until one end is done.
///
/// Returns the exit status. The loop owns both directions: a peer that sends
/// nothing must not stop input from reaching it, and input that never arrives
/// must not stop the peer's data from reaching standard output.
fn relay(sock: u64, opts: &Options) -> i32 {
    let mut buf = [0u8; 4096];
    let mut stdin_open = true;
    let stdout = io::stdout();

    loop {
        let readable = PollState {
            readable: true,
            ..Default::default()
        };
        let mut fds = vec![SelectFd {
            fd: sock,
            interests: readable,
            result: PollState::default(),
        }];
        if stdin_open {
            fds.push(SelectFd {
                fd: 0,
                interests: readable,
                result: PollState::default(),
            });
        }

        // After end of input the wait is bounded by -q, before it by -w. Both
        // default to waiting forever, which is the peer's turn to speak.
        let timeout = if stdin_open {
            opts.idle_ms
        } else {
            opts.quit_ms.min(opts.idle_ms)
        };
        let ready = poll(&mut fds, timeout);
        if ready < 0 {
            eprintln!("nc: poll failed");
            return 1;
        }
        if ready == 0 {
            return 0; // -w or -q expired
        }

        // A descriptor `poll` cannot watch reports invalid rather than staying
        // quiet; treating that as "never readable" would spin on it forever.
        if fds[0].result.invalid {
            eprintln!("nc: socket is not pollable");
            return 1;
        }
        if stdin_open && fds[1].result.invalid {
            stdin_open = false;
            let _ = net::shutdown(sock, net::SHUT_WR);
            continue;
        }

        // Hang-up is reported without being asked for, and it can arrive without
        // `readable` when the buffer is already drained; both mean a read that
        // returns end of file.
        if fds[0].result.readable || fds[0].result.hangup {
            match net::recv(sock, &mut buf) {
                Ok(0) => return 0,
                Ok(n) => {
                    let mut out = stdout.lock();
                    if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
                        return 1;
                    }
                }
                Err(_) => {
                    eprintln!("nc: receive failed");
                    return 1;
                }
            }
        }

        if stdin_open && (fds[1].result.readable || fds[1].result.hangup) {
            let n = sys_read(0, &mut buf);
            if n <= 0 {
                stdin_open = false;
                // Half-close: the peer sees end of input and can still answer.
                let _ = net::shutdown(sock, net::SHUT_WR);
                continue;
            }
            if net::send_all(sock, &buf[..n as usize]).is_err() {
                eprintln!("nc: send failed");
                return 1;
            }
        }
    }
}
