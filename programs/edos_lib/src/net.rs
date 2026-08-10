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

/// Mark a bound TCP socket as accepting connections. `backlog` is the number of
/// completed connections the kernel queues before it answers a SYN with RST.
pub fn listen(fd: u64, backlog: u32) -> Result<(), ()> {
    let ret = unsafe { sys::syscall2(sys::SYS_LISTEN, fd, backlog as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Take the next completed connection off a listening socket, blocking until one
/// arrives. Returns the new descriptor and the peer address.
pub fn accept(fd: u64) -> Result<(u64, SockAddrIn), ()> {
    let mut addr = SockAddrIn::new([0; 4], 0);
    let mut addr_len = core::mem::size_of::<SockAddrIn>() as u32;
    let ret = unsafe {
        sys::syscall3(
            sys::SYS_ACCEPT,
            fd,
            &mut addr as *mut SockAddrIn as u64,
            &mut addr_len as *mut u32 as u64,
        )
    };
    if ret == u64::MAX {
        Err(())
    } else {
        Ok((ret, addr))
    }
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

/// Resolve a host string to an IPv4 address.
///
/// An address is what the ICMP and raw-socket paths take; anything speaking TCP
/// or UDP should hand the name to `std::net` and let it resolve.
pub fn resolve_host(host: &str) -> Option<[u8; 4]> {
    use std::net::{SocketAddr, ToSocketAddrs};

    if let Some(literal) = parse_ipv4(host) {
        return Some(literal);
    }
    match (host, 0u16).to_socket_addrs().ok()?.next()? {
        SocketAddr::V4(v4) => Some(v4.ip().octets()),
        SocketAddr::V6(_) => None,
    }
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
