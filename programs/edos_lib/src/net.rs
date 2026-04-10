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

/// Resolve a hostname to an IPv4 address via DNS (using 10.0.2.3:53).
/// Returns None if resolution fails.
pub fn dns_resolve(hostname: &str) -> Option<[u8; 4]> {
    let fd = create_udp_socket().ok()?;
    let dns_server = SockAddrIn::new([10, 0, 2, 3], 53);

    // Random transaction ID
    let mut id_bytes = [0u8; 2];
    crate::getrandom(&mut id_bytes);
    let id = u16::from_ne_bytes(id_bytes);

    // Build DNS query
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

    if sendto(fd, &pkt, Some(&dns_server)).is_err() {
        close(fd);
        return None;
    }

    let mut resp = [0u8; 512];
    let len = match recvfrom(fd, &mut resp) {
        Ok(n) if n >= 12 => n,
        _ => {
            close(fd);
            return None;
        }
    };
    close(fd);

    // Parse: skip header (12) + question section, find first A record
    let data = &resp[..len];
    let ancount = u16::from_be_bytes([data[6], data[7]]);
    if ancount == 0 {
        return None;
    }
    let mut pos = 12;
    // Skip QNAME
    while pos < data.len() {
        let b = data[pos] as usize;
        if b == 0 {
            pos += 1;
            break;
        }
        if b >= 0xC0 {
            pos += 2;
            break;
        }
        pos += 1 + b;
    }
    pos += 4; // QTYPE + QCLASS
    // Parse answers
    for _ in 0..ancount {
        if pos + 12 > data.len() {
            break;
        }
        if data[pos] & 0xC0 == 0xC0 {
            pos += 2;
        } else {
            while pos < data.len() {
                let b = data[pos] as usize;
                if b == 0 {
                    pos += 1;
                    break;
                }
                pos += 1 + b;
            }
        }
        if pos + 10 > data.len() {
            break;
        }
        let rtype = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let rdlen = u16::from_be_bytes([data[pos + 8], data[pos + 9]]) as usize;
        pos += 10;
        if rtype == 1 && rdlen == 4 && pos + 4 <= data.len() {
            return Some([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        }
        pos += rdlen;
    }
    None
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
