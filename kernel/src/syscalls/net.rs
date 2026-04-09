//! Network socket syscalls: socket, bind, connect, sendto, recvfrom.

use alloc::sync::Arc;
use spin::Mutex;

use crate::{
    net::{
        socket::{
            AF_INET, SOCK_DGRAM, Socket, SocketAddr, SocketState, allocate_ephemeral_port,
            port_table,
        },
        stack::net_stack,
    },
    syscalls::Errno,
    thread::{pipe::FileDescriptor, scheduler::sched},
    util::uaccess::{try_copy_to_user, try_read_user},
};

pub use crate::net::socket::SockAddrIn;

pub fn sys_socket(domain: u64, sock_type: u64, _protocol: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if domain != AF_INET as u64 {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    if sock_type != SOCK_DGRAM as u64 {
        // Only UDP (SOCK_DGRAM) supported in this phase
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    let sock = Arc::new(Mutex::new(Socket::new_udp()));
    let fd_table = info.lock().fd_table.clone();
    let fd_num = fd_table.lock().allocate_fd(FileDescriptor::Socket(sock));
    fd_num as u64
}

pub fn sys_bind(fd: u64, addr_ptr: *const SockAddrIn, addr_len: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if addr_ptr.is_null() || addr_len < core::mem::size_of::<SockAddrIn>() as u64 {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let addr: SockAddrIn = match unsafe { try_read_user(addr_ptr) } {
        Some(a) => a,
        None => {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    };

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    let port = u16::from_be(addr.port);
    let ip = addr.addr;
    let local_addr = SocketAddr { ip, port };

    let mut s = sock_arc.lock();
    if s.closed {
        info.lock().errno = Errno::EBADF;
        return !0u64;
    }

    // Auto-assign ephemeral port if port 0 is requested
    let bind_port = if port == 0 {
        let proto = if s.sock_type == SOCK_DGRAM { 17u8 } else { 6u8 };
        match allocate_ephemeral_port(proto) {
            Some(p) => p,
            None => {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }
        }
    } else {
        port
    };

    let proto = if s.sock_type == SOCK_DGRAM { 17u8 } else { 6u8 };

    // Register in port table
    {
        let mut table = port_table().lock();
        if table.contains_key(&(proto, bind_port)) {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
        table.insert((proto, bind_port), sock_arc.clone());
    }

    s.local_addr = Some(SocketAddr {
        ip: local_addr.ip,
        port: bind_port,
    });
    s.state = SocketState::Bound;
    0
}

pub fn sys_connect(fd: u64, addr_ptr: *const SockAddrIn, addr_len: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if addr_ptr.is_null() || addr_len < core::mem::size_of::<SockAddrIn>() as u64 {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let addr: SockAddrIn = match unsafe { try_read_user(addr_ptr) } {
        Some(a) => a,
        None => {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    };

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    let port = u16::from_be(addr.port);
    let ip = addr.addr;

    let mut s = sock_arc.lock();
    if s.closed {
        info.lock().errno = Errno::EBADF;
        return !0u64;
    }

    // For UDP: if not yet bound, bind to an ephemeral port first
    if s.state == SocketState::Unbound {
        let proto = if s.sock_type == SOCK_DGRAM { 17u8 } else { 6u8 };
        if let Some(ep) = allocate_ephemeral_port(proto) {
            let local_addr = SocketAddr {
                ip: [0u8; 4],
                port: ep,
            };
            port_table().lock().insert((proto, ep), sock_arc.clone());
            s.local_addr = Some(local_addr);
            s.state = SocketState::Bound;
        }
    }

    s.remote_addr = Some(SocketAddr { ip, port });
    s.state = SocketState::Connected;
    0
}

pub fn sys_sendto(
    fd: u64,
    buf_ptr: *const u8,
    len: u64,
    _flags: u64,
    addr_ptr: *const SockAddrIn,
    addr_len: u64,
) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if buf_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let count = len as usize;
    if count == 0 {
        return 0;
    }

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    // Determine destination address
    let dst = if !addr_ptr.is_null() && addr_len >= core::mem::size_of::<SockAddrIn>() as u64 {
        let addr: SockAddrIn = match unsafe { try_read_user(addr_ptr) } {
            Some(a) => a,
            None => {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            }
        };
        SocketAddr {
            ip: addr.addr,
            port: u16::from_be(addr.port),
        }
    } else {
        // Use stored remote_addr (connected UDP)
        match sock_arc.lock().remote_addr {
            Some(a) => a,
            None => {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }
        }
    };

    // Copy data from userspace
    let mut data = alloc::vec![0u8; count];
    if !unsafe { crate::util::uaccess::try_copy_from_user(data.as_mut_ptr(), buf_ptr, count) } {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    // Get source port, auto-binding if needed
    let src_port = {
        let mut s = sock_arc.lock();
        if s.closed {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
        if s.local_addr.is_none() {
            let proto = if s.sock_type == SOCK_DGRAM { 17u8 } else { 6u8 };
            match allocate_ephemeral_port(proto) {
                Some(ep) => {
                    let local_addr = SocketAddr {
                        ip: [0u8; 4],
                        port: ep,
                    };
                    port_table().lock().insert((proto, ep), sock_arc.clone());
                    s.local_addr = Some(local_addr);
                    s.state = SocketState::Bound;
                    ep
                }
                None => {
                    info.lock().errno = Errno::EINVAL;
                    return !0u64;
                }
            }
        } else {
            s.local_addr.unwrap().port
        }
    };

    // Send via network stack
    let result = net_stack()
        .lock()
        .send_udp(src_port, dst.ip, dst.port, &data);

    match result {
        Ok(()) => count as u64,
        Err(_) => {
            info.lock().errno = Errno::EIO;
            !0u64
        }
    }
}

pub fn sys_recvfrom(
    fd: u64,
    buf_ptr: *mut u8,
    len: u64,
    _flags: u64,
    addr_ptr: *mut SockAddrIn,
    addr_len_ptr: *mut u32,
) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if buf_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let count = len as usize;
    if count == 0 {
        return 0;
    }

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    // Block until data arrives or socket is closed
    let rx_wq = sock_arc.lock().rx_wq.clone();
    rx_wq.wait_until(|| {
        let s = sock_arc.lock();
        !s.rx_queue.is_empty() || s.closed
    });

    let (data, src) = {
        let mut s = sock_arc.lock();
        if s.closed && s.rx_queue.is_empty() {
            return 0;
        }
        match s.rx_queue.pop_front() {
            Some(entry) => entry,
            None => {
                return 0;
            }
        }
    };

    let bytes_to_copy = data.len().min(count);
    if !unsafe { try_copy_to_user(buf_ptr, data.as_ptr(), bytes_to_copy) } {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    // Write source address if requested
    if !addr_ptr.is_null() {
        let sockaddr = SockAddrIn {
            family: AF_INET as u16,
            port: src.port.to_be(),
            addr: src.ip,
            zero: [0u8; 8],
        };
        let sockaddr_bytes = unsafe {
            core::slice::from_raw_parts(
                &sockaddr as *const SockAddrIn as *const u8,
                core::mem::size_of::<SockAddrIn>(),
            )
        };
        if !unsafe {
            try_copy_to_user(
                addr_ptr as *mut u8,
                sockaddr_bytes.as_ptr(),
                sockaddr_bytes.len(),
            )
        } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
        if !addr_len_ptr.is_null() {
            if !unsafe {
                crate::util::uaccess::try_write_user(
                    addr_len_ptr,
                    core::mem::size_of::<SockAddrIn>() as u32,
                )
            } {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            }
        }
    }

    bytes_to_copy as u64
}
