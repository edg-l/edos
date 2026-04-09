use std::env;

use edos_lib::net;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: dns <hostname>");
        return;
    }

    let hostname = &args[1];

    // Build DNS query
    let query = build_dns_query(hostname, 0x1234);

    // Send to QEMU's DNS server at 10.0.2.3:53
    let dns_server = [10, 0, 2, 3];
    let dns_addr = net::SockAddrIn::new(dns_server, 53);

    let fd = match net::create_udp_socket() {
        Ok(fd) => fd,
        Err(_) => {
            eprintln!("Failed to create socket");
            return;
        }
    };

    if net::sendto(fd, &query, Some(&dns_addr)).is_err() {
        eprintln!("Failed to send DNS query");
        net::close(fd);
        return;
    }

    let mut response = [0u8; 512];
    let len = match net::recvfrom(fd, &mut response) {
        Ok(n) => n,
        Err(_) => {
            eprintln!("Failed to receive DNS response");
            net::close(fd);
            return;
        }
    };

    net::close(fd);

    // Parse response
    if let Some(ip) = parse_dns_response(&response[..len]) {
        println!("{} -> {}.{}.{}.{}", hostname, ip[0], ip[1], ip[2], ip[3]);
    } else {
        eprintln!("No A record found for {}", hostname);
    }
}

fn build_dns_query(hostname: &str, id: u16) -> Vec<u8> {
    let mut pkt = Vec::new();

    // Header
    pkt.extend_from_slice(&id.to_be_bytes());
    pkt.extend_from_slice(&[0x01, 0x00]); // flags: standard query, recursion desired
    pkt.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT = 1
    pkt.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    pkt.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    pkt.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT

    // Question: encode hostname as labels (RFC 1035: max 63 bytes per label)
    for label in hostname.split('.') {
        let len = label.len().min(63);
        pkt.push(len as u8);
        pkt.extend_from_slice(&label.as_bytes()[..len]);
    }
    pkt.push(0); // root label

    pkt.extend_from_slice(&1u16.to_be_bytes()); // QTYPE = A
    pkt.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN

    pkt
}

fn parse_dns_response(data: &[u8]) -> Option<[u8; 4]> {
    if data.len() < 12 {
        return None;
    }

    let ancount = u16::from_be_bytes([data[6], data[7]]);
    if ancount == 0 {
        return None;
    }

    // Skip header (12 bytes) and question section
    let mut pos = 12;
    // Skip QNAME
    while pos < data.len() {
        let len = data[pos] as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len >= 0xC0 {
            pos += 2;
            break;
        } // compression pointer
        pos += 1 + len;
    }
    pos += 4; // skip QTYPE + QCLASS

    // Parse answers
    for _ in 0..ancount {
        if pos + 12 > data.len() {
            break;
        }

        // Skip name (may be compressed)
        if data[pos] & 0xC0 == 0xC0 {
            pos += 2;
        } else {
            while pos < data.len() {
                let len = data[pos] as usize;
                if len == 0 {
                    pos += 1;
                    break;
                }
                pos += 1 + len;
            }
        }

        if pos + 10 > data.len() {
            break;
        }
        let rtype = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let rdlength = u16::from_be_bytes([data[pos + 8], data[pos + 9]]) as usize;
        pos += 10;

        if rtype == 1 && rdlength == 4 && pos + 4 <= data.len() {
            return Some([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        }
        pos += rdlength;
    }

    None
}
