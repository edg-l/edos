//! High-level network socket wrappers.

use crate::{sys, time};

/// The errno a failing socket syscall returned.
///
/// The kernel answers a failed syscall with a negative errno and these
/// wrappers used to throw it away, so a caller could not tell a refused
/// connection from an unreachable host. It is carried instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetError(pub i64);

impl NetError {
    /// The errno, positive, as `strace` and `/proc` report it.
    pub fn errno(self) -> i64 {
        -self.0
    }
}

pub use syscall_abi::SockAddrIn;

pub fn create_udp_socket() -> Result<u64, NetError> {
    let fd = unsafe {
        sys::syscall3(
            sys::SYS_SOCKET,
            sys::AF_INET as u64,
            sys::SOCK_DGRAM as u64,
            0,
        )
    };
    if sys::is_err(fd) {
        Err(NetError(fd as i64))
    } else {
        Ok(fd)
    }
}

pub fn create_tcp_socket() -> Result<u64, NetError> {
    let fd = unsafe {
        sys::syscall3(
            sys::SYS_SOCKET,
            sys::AF_INET as u64,
            sys::SOCK_STREAM as u64,
            0,
        )
    };
    if sys::is_err(fd) {
        Err(NetError(fd as i64))
    } else {
        Ok(fd)
    }
}

pub fn connect(fd: u64, addr: &SockAddrIn) -> Result<(), NetError> {
    let ret = unsafe {
        sys::syscall3(
            sys::SYS_CONNECT,
            fd,
            addr as *const SockAddrIn as u64,
            core::mem::size_of::<SockAddrIn>() as u64,
        )
    };
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(())
    }
}

/// `getsockopt(SOL_SOCKET, SO_ERROR)`: the socket's pending error code, and
/// zero when it has none. This is how a non-blocking `connect` reports whether
/// the handshake `poll` just called writable succeeded or failed. The code is a
/// kernel `errno` number; `/proc/syscalls` names them.
pub fn so_error(fd: u64) -> Result<u32, NetError> {
    let mut val: i32 = 0;
    let mut len: u32 = core::mem::size_of::<i32>() as u32;
    let ret = unsafe {
        sys::syscall5(
            sys::SYS_GETSOCKOPT,
            fd,
            sys::SOL_SOCKET as u64,
            sys::SO_ERROR as u64,
            &mut val as *mut i32 as u64,
            &mut len as *mut u32 as u64,
        )
    };
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(val as u32)
    }
}

pub fn bind(fd: u64, addr: &SockAddrIn) -> Result<(), NetError> {
    let ret = unsafe {
        sys::syscall3(
            sys::SYS_BIND,
            fd,
            addr as *const SockAddrIn as u64,
            core::mem::size_of::<SockAddrIn>() as u64,
        )
    };
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(())
    }
}

/// Mark a bound TCP socket as accepting connections. `backlog` is the number of
/// completed connections the kernel queues before it answers a SYN with RST.
pub fn listen(fd: u64, backlog: u32) -> Result<(), NetError> {
    let ret = unsafe { sys::syscall2(sys::SYS_LISTEN, fd, backlog as u64) };
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(())
    }
}

/// Take the next completed connection off a listening socket, blocking until one
/// arrives. Returns the new descriptor and the peer address.
pub fn accept(fd: u64) -> Result<(u64, SockAddrIn), NetError> {
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
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok((ret, addr))
    }
}

/// Bound how long a `recvfrom` or `recv` on `fd` waits before giving up.
///
/// Zero clears the timeout and restores the blocking-forever default. Without
/// one, a datagram sent to a host that never answers costs the caller its
/// thread.
pub fn set_recv_timeout(fd: u64, millis: u64) -> Result<(), NetError> {
    #[repr(C)]
    struct Timeval {
        tv_sec: i64,
        tv_usec: i64,
    }

    let tv = Timeval {
        tv_sec: (millis / 1000) as i64,
        tv_usec: ((millis % 1000) * 1000) as i64,
    };
    let ret = unsafe {
        sys::syscall5(
            sys::SYS_SETSOCKOPT,
            fd,
            sys::SOL_SOCKET as u64,
            sys::SO_RCVTIMEO as u64,
            &tv as *const Timeval as u64,
            core::mem::size_of::<Timeval>() as u64,
        )
    };
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(())
    }
}

pub fn sendto(fd: u64, data: &[u8], addr: Option<&SockAddrIn>) -> Result<usize, NetError> {
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
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(ret as usize)
    }
}

/// Leave the datagram queued.
pub const MSG_PEEK: u64 = 0x2;
/// Report the datagram's real length rather than how much of it was copied.
pub const MSG_TRUNC: u64 = 0x20;
/// Fail with EAGAIN rather than blocking.
pub const MSG_DONTWAIT: u64 = 0x40;

/// `recvfrom` with flags and an explicit address capacity.
///
/// `addr` carries the caller's capacity in and the source address's real length
/// out, so a capacity below `size_of::<SockAddrIn>()` truncates the address and
/// still reports how long it was.
pub fn recvfrom_flags(
    fd: u64,
    buf: &mut [u8],
    flags: u64,
    addr: Option<(&mut SockAddrIn, &mut u32)>,
) -> Result<usize, NetError> {
    let (addr_ptr, addr_len_ptr) = match addr {
        Some((a, len)) => (a as *mut SockAddrIn as u64, len as *mut u32 as u64),
        None => (0, 0),
    };
    let ret = unsafe {
        sys::syscall6(
            sys::SYS_RECVFROM,
            fd,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            flags,
            addr_ptr,
            addr_len_ptr,
        )
    };
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(ret as usize)
    }
}

pub fn recvfrom(fd: u64, buf: &mut [u8]) -> Result<usize, NetError> {
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
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(ret as usize)
    }
}

pub fn send(fd: u64, data: &[u8]) -> Result<usize, NetError> {
    // Use write syscall for connected sockets
    let ret = unsafe { sys::syscall3(sys::SYS_WRITE, fd, data.as_ptr() as u64, data.len() as u64) };
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(ret as usize)
    }
}

pub fn recv(fd: u64, buf: &mut [u8]) -> Result<usize, NetError> {
    let ret =
        unsafe { sys::syscall3(sys::SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) };
    if ret as i64 <= 0 {
        if ret == 0 {
            Ok(0)
        } else {
            Err(NetError(ret as i64))
        }
    } else {
        Ok(ret as usize)
    }
}

/// Send every byte of `data`, retrying the short writes a stream socket makes.
///
/// A TCP write returns 0 when the send window is full rather than waiting, so a
/// caller that reads 0 as failure loses data the moment a peer stops reading as
/// fast as it is written. A millisecond is long enough for the ACK that reopens
/// the window; a peer that has gone away takes the connection out of
/// ESTABLISHED instead, and the write then fails outright.
pub fn send_all(fd: u64, data: &[u8]) -> Result<(), NetError> {
    let mut sent = 0;
    while sent < data.len() {
        match send(fd, &data[sent..]) {
            Ok(0) => {
                time::nanosleep(0, 1_000_000);
            }
            Ok(n) => sent += n,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `shutdown` directions, as in POSIX.
pub const SHUT_RD: u64 = 0;
pub const SHUT_WR: u64 = 1;
pub const SHUT_RDWR: u64 = 2;

/// Close one direction of a connected socket while leaving the descriptor open.
///
/// `SHUT_WR` sends a FIN, so the peer sees end of input and can answer before
/// the connection goes away; the read side keeps working until the peer closes
/// in turn. Without it a program that has no more to send has only `close`,
/// which discards the reply along with the connection.
pub fn shutdown(fd: u64, how: u64) -> Result<(), NetError> {
    let ret = unsafe { sys::syscall2(sys::SYS_SHUTDOWN, fd, how) };
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(())
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

/// The address name lookups are sent to: an override installed by
/// [`set_dns`] while its owner lives, and otherwise what DHCP learned.
pub fn get_dns() -> Option<[u8; 4]> {
    let mut addr = [0u8; 4];
    let ret = unsafe { sys::syscall1(sys::SYS_GETDNS, &mut addr as *mut [u8; 4] as u64) };
    if sys::is_err(ret) { None } else { Some(addr) }
}

/// Point every lookup on the machine at `addr`, or hand resolution back to the
/// DHCP-learned address by passing `0.0.0.0`.
///
/// The override belongs to the calling thread and is revoked when it exits, so
/// a resolver that dies does not take name resolution with it. `/proc/net`
/// reports both the override and the DHCP address it displaced.
pub fn set_dns(addr: [u8; 4]) -> Result<(), NetError> {
    let ret = unsafe { sys::syscall1(sys::SYS_SETDNS, &addr as *const [u8; 4] as u64) };
    if sys::is_err(ret) {
        Err(NetError(ret as i64))
    } else {
        Ok(())
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
    if sys::is_err(rtt) { None } else { Some(rtt) }
}
