//! High-level network socket wrappers.

use crate::sys;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrIn {
    pub family: u16,
    pub port: u16,
    pub addr: [u8; 4],
    pub zero: [u8; 8],
}

impl SockAddrIn {
    pub fn new(ip: [u8; 4], port: u16) -> Self {
        Self {
            family: sys::AF_INET as u16,
            port: port.to_be(),
            addr: ip,
            zero: [0; 8],
        }
    }
}

pub fn create_udp_socket() -> Result<u64, ()> {
    let fd = unsafe {
        sys::syscall3(
            sys::SYS_SOCKET,
            sys::AF_INET as u64,
            sys::SOCK_DGRAM as u64,
            0,
        )
    };
    if fd == u64::MAX { Err(()) } else { Ok(fd) }
}

pub fn create_tcp_socket() -> Result<u64, ()> {
    let fd = unsafe {
        sys::syscall3(
            sys::SYS_SOCKET,
            sys::AF_INET as u64,
            sys::SOCK_STREAM as u64,
            0,
        )
    };
    if fd == u64::MAX { Err(()) } else { Ok(fd) }
}

pub fn connect(fd: u64, addr: &SockAddrIn) -> Result<(), ()> {
    let ret = unsafe {
        sys::syscall3(
            sys::SYS_CONNECT,
            fd,
            addr as *const SockAddrIn as u64,
            core::mem::size_of::<SockAddrIn>() as u64,
        )
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn bind(fd: u64, addr: &SockAddrIn) -> Result<(), ()> {
    let ret = unsafe {
        sys::syscall3(
            sys::SYS_BIND,
            fd,
            addr as *const SockAddrIn as u64,
            core::mem::size_of::<SockAddrIn>() as u64,
        )
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sendto(fd: u64, data: &[u8], addr: Option<&SockAddrIn>) -> Result<usize, ()> {
    let (addr_ptr, addr_len) = match addr {
        Some(a) => (
            a as *const SockAddrIn as u64,
            core::mem::size_of::<SockAddrIn>() as u64,
        ),
        None => (0, 0),
    };
    let ret = unsafe {
        sys::syscall6(
            sys::SYS_SENDTO,
            fd,
            data.as_ptr() as u64,
            data.len() as u64,
            0,
            addr_ptr,
            addr_len,
        )
    };
    if ret == u64::MAX {
        Err(())
    } else {
        Ok(ret as usize)
    }
}

pub fn recvfrom(fd: u64, buf: &mut [u8]) -> Result<usize, ()> {
    let ret = unsafe {
        sys::syscall6(
            sys::SYS_RECVFROM,
            fd,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            0,
            0,
            0,
        )
    };
    if ret == u64::MAX {
        Err(())
    } else {
        Ok(ret as usize)
    }
}

pub fn send(fd: u64, data: &[u8]) -> Result<usize, ()> {
    // Use write syscall for connected sockets
    let ret = unsafe { sys::syscall3(sys::SYS_WRITE, fd, data.as_ptr() as u64, data.len() as u64) };
    if ret == u64::MAX {
        Err(())
    } else {
        Ok(ret as usize)
    }
}

pub fn recv(fd: u64, buf: &mut [u8]) -> Result<usize, ()> {
    let ret =
        unsafe { sys::syscall3(sys::SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) };
    if ret as i64 <= 0 {
        if ret == 0 { Ok(0) } else { Err(()) }
    } else {
        Ok(ret as usize)
    }
}

pub fn close(fd: u64) {
    unsafe { sys::syscall1(sys::SYS_CLOSE, fd) };
}

/// Parse a dotted-quad IPv4 literal.
pub fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
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

/// Resolve a host string: an IPv4 literal, `localhost`, or a DNS name.
pub fn resolve_host(host: &str) -> Option<[u8; 4]> {
    if host == "localhost" {
        return Some([127, 0, 0, 1]);
    }
    parse_ipv4(host).or_else(|| dns_resolve(host))
}

/// Why a DNS lookup produced no address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsError {
    Socket,
    Send,
    Recv,
    /// Response shorter than a DNS header, or an answer running past its end.
    Malformed,
    /// TC set: the answer did not fit in a UDP datagram (RFC 1035 4.1.1).
    Truncated,
    /// Non-zero RCODE (RFC 1035 4.1.1): 2 SERVFAIL, 3 NXDOMAIN, 5 REFUSED.
    Rcode(u8),
    /// The server answered, but with no A record.
    NoAddress,
}

impl core::fmt::Display for DnsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DnsError::Socket => f.write_str("cannot create socket"),
            DnsError::Send => f.write_str("cannot send query"),
            DnsError::Recv => f.write_str("no response from 10.0.2.3"),
            DnsError::Malformed => f.write_str("malformed response"),
            DnsError::Truncated => f.write_str("response truncated (TC set)"),
            DnsError::Rcode(2) => f.write_str("server failure (SERVFAIL)"),
            DnsError::Rcode(3) => f.write_str("no such domain (NXDOMAIN)"),
            DnsError::Rcode(5) => f.write_str("query refused (REFUSED)"),
            DnsError::Rcode(c) => write!(f, "server returned rcode {}", c),
            DnsError::NoAddress => f.write_str("no A record"),
        }
    }
}

/// Resolve a hostname to an IPv4 address via DNS (using 10.0.2.3:53).
pub fn dns_resolve(hostname: &str) -> Option<[u8; 4]> {
    dns_lookup(hostname).ok()
}

/// Resolve a hostname, reporting why the lookup failed.
pub fn dns_lookup(hostname: &str) -> Result<[u8; 4], DnsError> {
    let fd = create_udp_socket().map_err(|_| DnsError::Socket)?;
    let result = query_a_record(fd, hostname);
    close(fd);
    result
}

fn query_a_record(fd: u64, hostname: &str) -> Result<[u8; 4], DnsError> {
    let dns_server = SockAddrIn::new([10, 0, 2, 3], 53);

    let mut id_bytes = [0u8; 2];
    crate::getrandom(&mut id_bytes);
    let id = u16::from_ne_bytes(id_bytes);

    let query = build_a_query(hostname, id);
    sendto(fd, &query, Some(&dns_server)).map_err(|_| DnsError::Send)?;

    let mut resp = [0u8; 512];
    let len = recvfrom(fd, &mut resp).map_err(|_| DnsError::Recv)?;
    parse_a_response(&resp[..len])
}

/// Build a standard recursive query for the A record of `hostname`.
pub fn build_a_query(hostname: &str, id: u16) -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&id.to_be_bytes());
    pkt.extend_from_slice(&[0x01, 0x00]); // flags: recursion desired
    pkt.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
    pkt.extend_from_slice(&[0; 6]); // ANCOUNT, NSCOUNT, ARCOUNT = 0
    for label in hostname.split('.') {
        let len = label.len().min(63);
        pkt.push(len as u8);
        pkt.extend_from_slice(&label.as_bytes()[..len]);
    }
    pkt.push(0); // root label
    pkt.extend_from_slice(&1u16.to_be_bytes()); // QTYPE=A
    pkt.extend_from_slice(&1u16.to_be_bytes()); // QCLASS=IN
    pkt
}

/// Advance past a domain name at `pos`, following RFC 1035 4.1.4 message
/// compression: a name is a run of labels ending in either a root label or a
/// pointer, and a pointer may appear after any number of labels.
fn skip_name(data: &[u8], pos: &mut usize) -> Result<(), DnsError> {
    loop {
        let len = *data.get(*pos).ok_or(DnsError::Malformed)? as usize;
        if len & 0xC0 == 0xC0 {
            *pos += 2;
            return Ok(());
        }
        *pos += 1;
        if len == 0 {
            return Ok(());
        }
        *pos += len;
    }
}

/// Extract the first A record from a response to [`build_a_query`].
pub fn parse_a_response(data: &[u8]) -> Result<[u8; 4], DnsError> {
    if data.len() < 12 {
        return Err(DnsError::Malformed);
    }
    let flags = u16::from_be_bytes([data[2], data[3]]);
    if flags & 0x0200 != 0 {
        return Err(DnsError::Truncated);
    }
    let rcode = (flags & 0x000F) as u8;
    if rcode != 0 {
        return Err(DnsError::Rcode(rcode));
    }

    let qdcount = u16::from_be_bytes([data[4], data[5]]);
    let ancount = u16::from_be_bytes([data[6], data[7]]);
    if ancount == 0 {
        return Err(DnsError::NoAddress);
    }

    let mut pos = 12;
    for _ in 0..qdcount {
        skip_name(data, &mut pos)?;
        pos += 4; // QTYPE + QCLASS
    }

    for _ in 0..ancount {
        skip_name(data, &mut pos)?;
        // TYPE(2) CLASS(2) TTL(4) RDLENGTH(2)
        if pos + 10 > data.len() {
            return Err(DnsError::Malformed);
        }
        let rtype = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let rdlen = u16::from_be_bytes([data[pos + 8], data[pos + 9]]) as usize;
        pos += 10;
        if pos + rdlen > data.len() {
            return Err(DnsError::Malformed);
        }
        if rtype == 1 && rdlen == 4 {
            return Ok([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        }
        pos += rdlen;
    }

    Err(DnsError::NoAddress)
}

pub fn ping(dst_ip: [u8; 4], id: u16, seq: u16, timeout_ms: u64) -> Option<u64> {
    let rtt = unsafe {
        sys::syscall4(
            sys::SYS_PING,
            &dst_ip as *const [u8; 4] as u64,
            id as u64,
            seq as u64,
            timeout_ms,
        )
    };
    if rtt == u64::MAX { None } else { Some(rtt) }
}
