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
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
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
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

pub fn send(fd: u64, data: &[u8]) -> Result<usize, ()> {
    // Use write syscall for connected sockets
    let ret =
        unsafe { sys::syscall3(sys::SYS_WRITE, fd, data.as_ptr() as u64, data.len() as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
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
