//! Socket ABI checks: receive flags, and the value-result address length.
//!
//! Needs a reachable DNS server to produce one real datagram; QEMU user
//! networking answers on 10.0.2.3:53, which is the default.

use std::env;
use std::process;

use edos_lib::net::{self, MSG_DONTWAIT, MSG_PEEK, MSG_TRUNC, SockAddrIn};

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
    println!("{passed} passed, {failed} failed");
    if failed > 0 {
        process::exit(1);
    }
}
