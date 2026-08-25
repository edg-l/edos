//! `sshd` — an SSH-2 server.
//!
//! One algorithm per role, all of them ones OpenSSH still offers by default:
//! `curve25519-sha256` key exchange, an `ssh-ed25519` host key, `aes128-ctr`
//! and `hmac-sha2-256`. There is no bignum arithmetic anywhere on this system,
//! which is why there is no RSA host key and no `diffie-hellman-group14`.
//!
//! One thread per connection, as in `httpd`: a client that opens a socket and
//! then says nothing cannot stop the next one from being served.

mod auth;
mod config;
mod error;
mod hostkey;
mod kex;
mod packet;
mod session;
mod wire;

use std::env;
use std::process::exit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use edos_lib::net::{self, SockAddrIn};
use edos_lib::process;

use config::{Config, DEFAULT_CONFIG};
use error::{Error, Result};
use hostkey::HostKey;
use packet::{DISCONNECT_PROTOCOL_ERROR, Transport};
use session::Session;

/// Connections served at once. Each costs a thread and a shell, so this is a
/// bound on the machine rather than on the protocol.
const MAX_CONNECTIONS: usize = 16;

/// How long an unauthenticated peer may leave the server waiting, in
/// milliseconds. OpenSSH calls this `LoginGraceTime` and defaults to two
/// minutes; this is a smaller machine.
const LOGIN_GRACE_MS: u64 = 30_000;

fn usage() -> ! {
    eprintln!("usage: sshd [-p port] [-f config] [-u user:password] [-k hostkey] [-s shell] [-v]");
    eprintln!("  -p port       port to listen on (default 22)");
    eprintln!("  -f config     configuration file (default {DEFAULT_CONFIG})");
    eprintln!("  -u user:pass  allow a user, overriding the file's entry");
    eprintln!("  -k hostkey    ed25519 host key, created on first use");
    eprintln!("  -s shell      shell to run (default /bin/sh)");
    eprintln!("  -v            log each connection");
    exit(2)
}

/// The argument after the flag at `i`, advancing past it.
fn value(args: &[String], i: &mut usize) -> String {
    *i += 1;
    args.get(*i).cloned().unwrap_or_else(|| usage())
}

fn parse_args() -> Config {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut cfg = Config::default();

    // The file is read first so that a flag always wins, which means finding
    // -f before anything else is applied.
    let mut config_path = DEFAULT_CONFIG.to_string();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-f" {
            config_path = args.get(i + 1).cloned().unwrap_or_else(|| usage());
        }
        i += 1;
    }
    if let Err(e) = cfg.merge_file(&config_path) {
        eprintln!("sshd: {e}");
        exit(2);
    }

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                let v = value(&args, &mut i);
                cfg.port = v.parse().unwrap_or_else(|_| {
                    eprintln!("sshd: {v:?} is not a port");
                    exit(2)
                });
            }
            "-f" => {
                value(&args, &mut i);
            }
            "-k" => cfg.hostkey = value(&args, &mut i),
            "-s" => cfg.shell = value(&args, &mut i),
            "-u" => {
                let v = value(&args, &mut i);
                let Some((name, password)) = v.split_once(':') else {
                    eprintln!("sshd: -u wants user:password");
                    exit(2);
                };
                cfg.add_user(name, password);
            }
            "-v" => cfg.verbose = true,
            "-h" | "--help" => usage(),
            other => {
                eprintln!("sshd: unknown option {other:?}");
                usage()
            }
        }
        i += 1;
    }
    cfg
}

fn main() {
    let cfg = Arc::new(parse_args());
    if cfg.users.is_empty() {
        eprintln!("sshd: no users configured; nothing could authenticate");
        eprintln!("      add `user <name> <password>` to {DEFAULT_CONFIG} or pass -u");
        exit(2);
    }

    let host = match HostKey::load_or_create(&cfg.hostkey) {
        Ok(k) => Arc::new(k),
        Err(e) => {
            eprintln!("sshd: host key {}: {e}", cfg.hostkey);
            exit(1);
        }
    };
    println!("sshd: host key {}", host.fingerprint());
    if cfg.verbose {
        // The `known_hosts` line, so a first connection can be verified against
        // the server rather than accepted on trust.
        println!("sshd: {}", host.public_line());
    }

    let Ok(listener) = net::create_tcp_socket() else {
        eprintln!("sshd: cannot create a socket");
        exit(1);
    };
    if net::bind(listener, &SockAddrIn::new([0, 0, 0, 0], cfg.port)).is_err() {
        eprintln!("sshd: cannot bind port {}", cfg.port);
        exit(1);
    }
    if net::listen(listener, 8).is_err() {
        eprintln!("sshd: cannot listen on port {}", cfg.port);
        exit(1);
    }
    println!("sshd: listening on port {}", cfg.port);

    let live = Arc::new(AtomicUsize::new(0));

    loop {
        let Ok((sock, peer)) = net::accept(listener) else {
            // A failed accept is not fatal: the listener is still good, and the
            // next client should not be refused because this one went away.
            continue;
        };

        // A connection costs a thread for as long as it lasts, so the count is
        // the resource to bound. Refusing the surplus keeps a flood from
        // reaching `thread::spawn` at all.
        if live.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            eprintln!("sshd: refusing a connection: {MAX_CONNECTIONS} already open");
            net::close(sock);
            continue;
        }

        let cfg = Arc::clone(&cfg);
        let host = Arc::clone(&host);
        let thread_live = Arc::clone(&live);
        let peer_ip = peer.addr;
        live.fetch_add(1, Ordering::Relaxed);

        // `thread::Builder` rather than `thread::spawn`, which panics when a
        // thread cannot be created. That panic would land on the accept loop
        // and take the listener down with it, so one client's arrival at a bad
        // moment would end the service for everyone.
        let spawned = thread::Builder::new().spawn(move || {
            // Per thread, because a new thread starts with default signal
            // dispositions. Without this, writing to a shell that has already
            // exited kills this thread outright: the pipe has no reader, the
            // kernel sends SIGPIPE, and its default action is to terminate.
            // The socket would then never be closed, since nothing unwinds.
            let _ = process::sys_sigaction(process::SIGPIPE, process::SIG_IGN as u64);

            if cfg.verbose {
                println!(
                    "sshd: connection from {}.{}.{}.{}",
                    peer_ip[0], peer_ip[1], peer_ip[2], peer_ip[3]
                );
            }
            match serve(sock, &cfg, &host) {
                Ok(user) => {
                    if cfg.verbose {
                        println!("sshd: session for {user} ended");
                    }
                }
                // A client that hangs up is the ordinary end of a session, so
                // it is only worth a line when connections are being logged.
                Err(e @ (Error::PeerDisconnect(_) | Error::Io(_))) => {
                    if cfg.verbose {
                        println!("sshd: {e}");
                    }
                }
                Err(e) => eprintln!("sshd: {e}"),
            }
            net::close(sock);
            thread_live.fetch_sub(1, Ordering::Relaxed);
        });
        if spawned.is_err() {
            eprintln!("sshd: cannot start a thread for this connection");
            net::close(sock);
            live.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// The whole protocol for one connection, in the order it happens.
fn serve(sock: u64, cfg: &Config, host: &HostKey) -> Result<String> {
    // The login grace period. Everything up to a successful authentication is
    // work an unauthenticated peer can make this thread do, and a client that
    // connects and then says nothing would otherwise hold a thread forever.
    // Cleared once the session starts, where an idle connection is legitimate:
    // a shell waiting for its user is exactly that.
    let _ = net::set_recv_timeout(sock, LOGIN_GRACE_MS);

    let mut t = Transport::new(sock);
    let versions = kex::exchange_versions(&t)?;
    if let Err(e) = kex::run(&mut t, host, &versions, None) {
        t.disconnect(DISCONNECT_PROTOCOL_ERROR, &e.to_string());
        return Err(e);
    }
    let user = auth::run(&mut t, cfg)?;

    let _ = net::set_recv_timeout(sock, 0);
    Session::new(&mut t, cfg, host, &versions, user.clone()).run()?;
    Ok(user)
}
