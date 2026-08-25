//! Socket ABI checks: receive flags, the value-result address length, and the
//! non-blocking connect handshake.
//!
//! Needs a reachable DNS server to produce one real datagram; QEMU user
//! networking answers on 10.0.2.3:53, which is the default. The connect cases
//! run over 127.0.0.1 and need no network at all.

use std::env;
use std::process;

use edos_lib::io::{self, PollState, SelectFd};
use edos_lib::net::{self, MSG_DONTWAIT, MSG_PEEK, MSG_TRUNC, SockAddrIn};
use edos_lib::process::set_nonblocking;
use edos_lib::trace::read_syscall_table;

const SOCKADDR_LEN: u32 = core::mem::size_of::<SockAddrIn>() as u32;

fn check(passed: &mut u32, failed: &mut u32, name: &str, ok: bool, detail: String) {
    if ok {
        *passed += 1;
        println!("ok   {name}: {detail}");
    } else {
        *failed += 1;
        println!("FAIL {name}: {detail}");
    }
}

/// A DNS query for `example.com`, the smallest thing that reliably yields one
/// datagram of a few hundred bytes.
fn query() -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&0x5a5au16.to_be_bytes());
    pkt.extend_from_slice(&[0x01, 0x00]);
    pkt.extend_from_slice(&1u16.to_be_bytes());
    pkt.extend_from_slice(&[0; 6]);
    for label in "example.com".split('.') {
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.push(0);
    pkt.extend_from_slice(&1u16.to_be_bytes());
    pkt.extend_from_slice(&1u16.to_be_bytes());
    pkt
}

/// Wait for `fd` to become writable, which for a socket mid-handshake is the
/// handshake resolving, either way.
fn wait_writable(fd: u64, timeout_ms: u64) -> PollState {
    let mut fds = [SelectFd {
        fd,
        interests: PollState {
            writable: true,
            ..PollState::default()
        },
        result: PollState::default(),
    }];
    io::poll(&mut fds, timeout_ms);
    fds[0].result
}

/// The non-blocking connect contract: `EINPROGRESS` at once rather than a wait,
/// `poll` reporting the socket writable when the handshake resolves, and
/// `SO_ERROR` carrying which way it went.
///
/// Loopback, so this needs no network: a listener is one bind away and a port
/// with nothing on it answers RST. Loopback also delivers inside the sending
/// syscall, so a connect that has already resolved by the time it returns is
/// allowed to say so; what is not allowed is blocking, or losing the outcome.
fn connect_cases(passed: &mut u32, failed: &mut u32) {
    const LISTEN_PORT: u16 = 7877;
    const DEAD_PORT: u16 = 7878;

    let table = read_syscall_table();
    let einprogress = table.errno_value("EINPROGRESS").unwrap_or(u32::MAX);
    let econnrefused = table.errno_value("ECONNREFUSED").unwrap_or(u32::MAX);
    let eisconn = table.errno_value("EISCONN").unwrap_or(u32::MAX);
    let local = |port| SockAddrIn::new([127, 0, 0, 1], port);

    let (Ok(listener), Ok(fd)) = (net::create_tcp_socket(), net::create_tcp_socket()) else {
        check(passed, failed, "tcp sockets", false, "socket failed".into());
        return;
    };
    if net::bind(listener, &local(LISTEN_PORT)).is_err() || net::listen(listener, 4).is_err() {
        check(
            passed,
            failed,
            "listen 127.0.0.1",
            false,
            "bind failed".into(),
        );
        net::close(listener);
        net::close(fd);
        return;
    }

    let _ = set_nonblocking(fd, true);
    let started = net::connect(fd, &local(LISTEN_PORT));
    let code = io::last_errno_raw();
    check(
        passed,
        failed,
        "nonblocking connect does not wait",
        started.is_ok() || code == einprogress,
        format!("{started:?} errno {code}"),
    );

    let ready = wait_writable(fd, 3000);
    check(
        passed,
        failed,
        "poll reports the handshake",
        ready.writable,
        format!("writable {} error {}", ready.writable, ready.error),
    );
    check(
        passed,
        failed,
        "so_error clear on success",
        net::so_error(fd) == Ok(0),
        format!("{:?}", net::so_error(fd)),
    );

    // A second connect on a socket that already has one reports the outcome
    // rather than starting another handshake.
    let again = net::connect(fd, &local(LISTEN_PORT));
    let again_code = io::last_errno_raw();
    check(
        passed,
        failed,
        "connect on a connected socket",
        again.is_err() && again_code == eisconn,
        format!("{again:?} errno {again_code}"),
    );

    // The connection is real: the listener has it, and a byte crosses it. The
    // listener is non-blocking so a handshake that never landed fails the check
    // instead of hanging the test.
    let _ = set_nonblocking(listener, true);
    let accepted = if io::poll_readable(listener, 2000) {
        net::accept(listener)
    } else {
        // Nothing to accept within the timeout. ETIMEDOUT is the closest
        // errno to "poll gave up", and no caller here reads it.
        Err(net::NetError(-110))
    };
    let round_trip = match accepted {
        Ok((peer, _)) => {
            let sent = net::send(fd, b"x");
            let mut buf = [0u8; 1];
            let got = net::recv(peer, &mut buf);
            net::close(peer);
            sent == Ok(1) && got == Ok(1) && buf[0] == b'x'
        }
        Err(_) => false,
    };
    check(
        passed,
        failed,
        "accepted and carries data",
        round_trip,
        format!("accept {:?}", accepted.map(|(p, _)| p)),
    );
    net::close(fd);

    // A port with nothing listening: the failure has to reach the caller too.
    let Ok(dead) = net::create_tcp_socket() else {
        check(passed, failed, "tcp socket", false, "socket failed".into());
        net::close(listener);
        return;
    };
    let _ = set_nonblocking(dead, true);
    let refused = net::connect(dead, &local(DEAD_PORT));
    let refused_code = io::last_errno_raw();
    let outcome = if refused.is_err() && refused_code == econnrefused {
        econnrefused
    } else if refused.is_err() && refused_code == einprogress {
        wait_writable(dead, 3000);
        net::so_error(dead).unwrap_or(u32::MAX)
    } else {
        0
    };
    check(
        passed,
        failed,
        "refused connect reports ECONNREFUSED",
        outcome == econnrefused,
        format!("{refused:?} errno {refused_code}, so_error {outcome}"),
    );
    net::close(dead);

    // An address nothing answers: the handshake stays outstanding, which is the
    // only way to see EINPROGRESS itself, since loopback resolves inside the
    // call. Poll must withhold writable for as long as that lasts.
    let Ok(pending) = net::create_tcp_socket() else {
        check(passed, failed, "tcp socket", false, "socket failed".into());
        net::close(listener);
        return;
    };
    let _ = set_nonblocking(pending, true);
    let started = net::connect(pending, &SockAddrIn::new([10, 0, 2, 99], 9));
    let started_code = io::last_errno_raw();
    let stalled = wait_writable(pending, 500);
    check(
        passed,
        failed,
        "EINPROGRESS while the handshake is outstanding",
        started.is_err() && started_code == einprogress && !stalled.writable,
        format!(
            "{started:?} errno {started_code}, writable {}",
            stalled.writable
        ),
    );
    net::close(pending);

    // Blocking connect is unchanged.
    let Ok(blocking) = net::create_tcp_socket() else {
        check(passed, failed, "tcp socket", false, "socket failed".into());
        net::close(listener);
        return;
    };
    let done = net::connect(blocking, &local(LISTEN_PORT));
    check(
        passed,
        failed,
        "blocking connect still connects",
        done.is_ok(),
        format!("{done:?}"),
    );
    if io::poll_readable(listener, 2000)
        && let Ok((peer, _)) = net::accept(listener)
    {
        net::close(peer);
    }
    net::close(blocking);
    net::close(listener);
}

/// A datagram to 127.0.0.1 and back.
///
/// The stack rewrites the source of a loopback packet to 127.0.0.1, so a
/// checksum computed from the interface address covers an address that never
/// appeared on the datagram and the receiver discards it. Needs no network.
fn udp_loopback_case(passed: &mut u32, failed: &mut u32) {
    const PORT: u16 = 7879;

    let Ok(fd) = net::create_udp_socket() else {
        check(passed, failed, "udp socket", false, "socket failed".into());
        return;
    };
    if net::bind(fd, &SockAddrIn::new([0, 0, 0, 0], PORT)).is_err() {
        check(passed, failed, "udp bind", false, format!("port {PORT}"));
        net::close(fd);
        return;
    }

    let payload = b"loopback datagram";
    let sent = net::sendto(fd, payload, Some(&SockAddrIn::new([127, 0, 0, 1], PORT)));

    // Loopback delivers inside the sending syscall, so the datagram is already
    // queued; MSG_DONTWAIT keeps a lost one from hanging the test.
    let mut buf = [0u8; 128];
    let mut addr = SockAddrIn::new([0; 4], 0);
    let mut addr_len = SOCKADDR_LEN;
    let got = net::recvfrom_flags(fd, &mut buf, MSG_DONTWAIT, Some((&mut addr, &mut addr_len)));

    check(
        passed,
        failed,
        "udp loopback round trip",
        sent == Ok(payload.len()) && got == Ok(payload.len()) && &buf[..payload.len()] == payload,
        format!("sent {sent:?}, received {got:?}"),
    );
    check(
        passed,
        failed,
        "udp loopback source",
        addr.addr == [127, 0, 0, 1] && u16::from_be(addr.port) == PORT,
        format!("from {:?}:{}", addr.addr, u16::from_be(addr.port)),
    );

    net::close(fd);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let server = args
        .get(1)
        .and_then(|s| net::parse_ipv4(s))
        .unwrap_or([10, 0, 2, 3]);

    let Ok(fd) = net::create_udp_socket() else {
        eprintln!("socket failed");
        process::exit(1);
    };
    let dst = SockAddrIn::new(server, 53);
    if net::sendto(fd, &query(), Some(&dst)).is_err() {
        eprintln!("send failed");
        net::close(fd);
        process::exit(1);
    }
    // A lost query must not hang the test.
    let _ = net::set_recv_timeout(fd, 3000);

    let (mut passed, mut failed) = (0u32, 0u32);

    let mut buf = [0u8; 1500];
    let mut addr = SockAddrIn::new([0; 4], 0);
    let mut addr_len = SOCKADDR_LEN;
    let peeked = match net::recvfrom_flags(fd, &mut buf, MSG_PEEK, Some((&mut addr, &mut addr_len)))
    {
        Ok(n) => n,
        Err(_) => {
            eprintln!("no response from {server:?}:53");
            net::close(fd);
            process::exit(1);
        }
    };
    check(
        &mut passed,
        &mut failed,
        "peek addr",
        addr.addr == server && u16::from_be(addr.port) == 53 && addr_len == SOCKADDR_LEN,
        format!(
            "src {:?}:{} len {addr_len}",
            addr.addr,
            u16::from_be(addr.port)
        ),
    );

    // MSG_TRUNC over a buffer far too small: the copy is bounded by the buffer,
    // the return value is the datagram.
    let mut small = [0u8; 4];
    let truncated = net::recvfrom_flags(fd, &mut small, MSG_PEEK | MSG_TRUNC, None);
    check(
        &mut passed,
        &mut failed,
        "msg_trunc",
        truncated == Ok(peeked),
        format!("{truncated:?} against a {peeked}-byte datagram"),
    );

    // A capacity below a whole sockaddr truncates the address and still reports
    // the untruncated length, which is how the caller learns it was short.
    let mut short = SockAddrIn::new([0; 4], 0);
    short.zero = [0xaa; 8];
    let mut short_len = 8u32;
    let _ = net::recvfrom_flags(fd, &mut buf, MSG_PEEK, Some((&mut short, &mut short_len)));
    check(
        &mut passed,
        &mut failed,
        "short addr_len",
        short.addr == server && short.zero == [0xaa; 8] && short_len == SOCKADDR_LEN,
        format!(
            "wrote {:?}, tail {:#x}, reported {short_len}",
            short.addr, short.zero[0]
        ),
    );

    // Every peek so far left the datagram queued, so the plain receive still
    // finds it, and only then is the queue empty.
    let consumed = net::recvfrom(fd, &mut buf);
    check(
        &mut passed,
        &mut failed,
        "peek leaves the datagram",
        consumed == Ok(peeked),
        format!("{consumed:?} after three peeks of {peeked} bytes"),
    );

    let empty = net::recvfrom_flags(fd, &mut buf, MSG_DONTWAIT, None);
    check(
        &mut passed,
        &mut failed,
        "dontwait on an empty queue",
        empty.is_err(),
        format!("{empty:?}"),
    );

    // 0x1 is MSG_OOB, which this stack does not implement.
    let unknown = net::recvfrom_flags(fd, &mut buf, 0x1, None);
    check(
        &mut passed,
        &mut failed,
        "unimplemented flag refused",
        unknown.is_err(),
        format!("{unknown:?}"),
    );

    net::close(fd);

    connect_cases(&mut passed, &mut failed);
    udp_loopback_case(&mut passed, &mut failed);

    println!("{passed} passed, {failed} failed");
    if failed > 0 {
        process::exit(1);
    }
}
