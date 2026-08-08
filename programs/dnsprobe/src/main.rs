//! Dump a raw DNS response, for comparing a parser against real bytes.

use std::env;

use edos_lib::net::{self, SockAddrIn};

fn main() {
    let args: Vec<String> = env::args().collect();
    let host = args.get(1).map(|s| s.as_str()).unwrap_or("example.com");
    let server = args
        .get(2)
        .and_then(|s| net::parse_ipv4(s))
        .unwrap_or([10, 0, 2, 3]);

    let mut pkt = Vec::new();
    pkt.extend_from_slice(&0x1234u16.to_be_bytes());
    pkt.extend_from_slice(&[0x01, 0x00]);
    pkt.extend_from_slice(&1u16.to_be_bytes());
    pkt.extend_from_slice(&[0; 6]);
    for label in host.split('.') {
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.push(0);
    pkt.extend_from_slice(&1u16.to_be_bytes());
    pkt.extend_from_slice(&1u16.to_be_bytes());

    let Ok(fd) = net::create_udp_socket() else {
        eprintln!("socket failed");
        return;
    };
    let addr = SockAddrIn::new(server, 53);
    if net::sendto(fd, &pkt, Some(&addr)).is_err() {
        eprintln!("send failed");
        net::close(fd);
        return;
    }
    let mut resp = [0u8; 1500];
    match net::recvfrom(fd, &mut resp) {
        Ok(n) => {
            println!("len={}", n);
            let mut line = String::new();
            for b in &resp[..n] {
                line.push_str(&format!("{:02x}", b));
            }
            println!("{}", line);
        }
        Err(_) => eprintln!("recv failed"),
    }
    net::close(fd);
}
