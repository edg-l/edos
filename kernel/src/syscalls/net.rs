//! Network socket syscalls: socket, bind, connect, sendto, recvfrom.

use alloc::sync::Arc;
use spin::Mutex;

use core::time::Duration;

use crate::{
    net::{
        ipv4,
        socket::{
            AF_INET, SOCK_DGRAM, SOCK_STREAM, Socket, SocketAddr, SocketState,
            allocate_ephemeral_port, port_table,
        },
        stack::net_stack,
        tcp::{TcpConnection, TcpState},
    },
    syscalls::Errno,
    thread::{pipe::FileDescriptor, scheduler::sched},
    util::uaccess::{try_copy_to_user, try_read_user, try_write_user},
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

    let sock = match sock_type as u32 {
        SOCK_DGRAM => Arc::new(Mutex::new(Socket::new_udp())),
        SOCK_STREAM => Arc::new(Mutex::new(Socket::new_tcp())),
        _ => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };
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

    // Read socket state without holding the lock across port_table access.
    let (closed, sock_type) = {
        let s = sock_arc.lock();
        (s.closed, s.sock_type)
    };
    if closed {
        info.lock().errno = Errno::EBADF;
        return !0u64;
    }

    let proto = if sock_type == SOCK_DGRAM { 17u8 } else { 6u8 };

    // Auto-assign ephemeral port if port 0 is requested
    let bind_port = if port == 0 {
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

    // Register in port table (lock port_table first, then socket -- consistent order).
    {
        let mut table = port_table().lock();
        if table.contains_key(&(proto, bind_port)) {
            info.lock().errno = Errno::EADDRINUSE;
            return !0u64;
        }
        table.insert((proto, bind_port), sock_arc.clone());
    }

    let mut s = sock_arc.lock();
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

    let sock_type = {
        let s = sock_arc.lock();
        if s.closed {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
        s.sock_type
    };

    if sock_type == SOCK_STREAM {
        // TCP active open: auto-bind if needed, build SYN, wait for Established
        let local_port = {
            let mut s = sock_arc.lock();
            if s.state == SocketState::Unbound {
                match allocate_ephemeral_port(6u8) {
                    Some(ep) => {
                        let local_addr = SocketAddr {
                            ip: [0u8; 4],
                            port: ep,
                        };
                        port_table().lock().insert((6u8, ep), sock_arc.clone());
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
                match s.local_addr {
                    Some(a) => a.port,
                    None => {
                        info.lock().errno = Errno::EINVAL;
                        return !0u64;
                    }
                }
            }
        };

        let local_ip = net_stack().lock().local_ip;
        let remote_sa = SocketAddr { ip, port };
        let local_sa = SocketAddr {
            ip: local_ip,
            port: local_port,
        };

        let mut conn = TcpConnection::new(local_ip, local_port, ip, port);
        let syn_seg = conn.build_syn();
        let conn_arc = Arc::new(Mutex::new(conn));

        // Send SYN, retrying once if ARP is pending
        {
            let mut stack = net_stack().lock();
            stack
                .tcp_connections
                .insert((local_sa, remote_sa), conn_arc.clone());
            if stack.send_ip(ip, ipv4::IpProtocol::Tcp, &syn_seg).is_err() {
                let resolve_ip = if stack.is_local_subnet(&ip) {
                    ip
                } else {
                    stack.gateway_ip
                };
                let arp_wq = stack.arp_cache.get_or_create_waiter(resolve_ip);
                drop(stack);
                arp_wq.wait_until_timeout(
                    || net_stack().lock().arp_cache.lookup(&resolve_ip).is_some(),
                    Some(Duration::from_millis(200)),
                );
                let _ = net_stack()
                    .lock()
                    .send_ip(ip, ipv4::IpProtocol::Tcp, &syn_seg);
            }
        }

        sock_arc.lock().tcp_conn = Some(conn_arc.clone());

        let state_wq = conn_arc.lock().state_wq.clone();
        state_wq.wait_until_timeout(
            || {
                let c = conn_arc.lock();
                c.state == TcpState::Established || c.state == TcpState::Closed
            },
            Some(Duration::from_secs(5)),
        );

        let state = conn_arc.lock().state;
        if state == TcpState::Established {
            sock_arc.lock().state = SocketState::Connected;
            0
        } else {
            // Connection failed, clean up
            net_stack()
                .lock()
                .tcp_connections
                .remove(&(local_sa, remote_sa));
            sock_arc.lock().tcp_conn = None;
            info.lock().errno = Errno::ECONNREFUSED;
            !0u64
        }
    } else {
        // UDP connect: just set remote_addr
        let mut s = sock_arc.lock();
        if s.state == SocketState::Unbound {
            match allocate_ephemeral_port(17u8) {
                Some(ep) => {
                    let local_addr = SocketAddr {
                        ip: [0u8; 4],
                        port: ep,
                    };
                    port_table().lock().insert((17u8, ep), sock_arc.clone());
                    s.local_addr = Some(local_addr);
                    s.state = SocketState::Bound;
                }
                None => {
                    info.lock().errno = Errno::EINVAL;
                    return !0u64;
                }
            }
        }
        s.remote_addr = Some(SocketAddr { ip, port });
        s.state = SocketState::Connected;
        0
    }
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

    // Copy data from userspace (cap to prevent OOM from malicious count)
    const MAX_SENDTO_SIZE: usize = 65536; // 64 KiB
    let count = count.min(MAX_SENDTO_SIZE);
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

    // Send via network stack, retrying on ARP pending
    for attempt in 0..3u32 {
        let result = net_stack()
            .lock()
            .send_udp(src_port, dst.ip, dst.port, &data);
        match result {
            Ok(()) => return count as u64,
            Err("arp pending") => {
                // Wait for ARP resolution
                let arp_wq = {
                    let mut stack = net_stack().lock();
                    let resolve_ip = if stack.is_local_subnet(&dst.ip) {
                        dst.ip
                    } else {
                        stack.gateway_ip
                    };
                    stack.arp_cache.get_or_create_waiter(resolve_ip)
                };
                arp_wq.wait_until_timeout(
                    || {
                        let stack = net_stack().lock();
                        let resolve_ip = if stack.is_local_subnet(&dst.ip) {
                            dst.ip
                        } else {
                            stack.gateway_ip
                        };
                        stack.arp_cache.lookup(&resolve_ip).is_some()
                    },
                    Some(Duration::from_millis(100)),
                );
                crate::log!("net: sendto ARP attempt {}", attempt + 1);
            }
            Err(_) => {
                info.lock().errno = Errno::EIO;
                return !0u64;
            }
        }
    }
    info.lock().errno = Errno::EIO;
    !0u64
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

pub fn sys_listen(fd: u64, backlog: u32) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    let mut s = sock_arc.lock();

    if s.sock_type != SOCK_STREAM {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }
    if s.closed {
        info.lock().errno = Errno::EBADF;
        return !0u64;
    }
    if s.state == SocketState::Unbound {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    // Register in port table under TCP protocol if not already present
    if let Some(local_addr) = s.local_addr {
        let mut table = port_table().lock();
        if !table.contains_key(&(6u8, local_addr.port)) {
            table.insert((6u8, local_addr.port), sock_arc.clone());
        }
    }

    s.listening = true;
    s.backlog = if backlog == 0 { 1 } else { backlog };
    0
}

pub fn sys_accept(fd: u64, addr_ptr: *mut SockAddrIn, addr_len_ptr: *mut u32) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    {
        let s = sock_arc.lock();
        if s.sock_type != SOCK_STREAM || !s.listening {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
        if s.closed {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    }

    // Block until accept_queue has an Established connection
    let rx_wq = sock_arc.lock().rx_wq.clone();
    rx_wq.wait_until(|| {
        let s = sock_arc.lock();
        s.accept_queue
            .iter()
            .any(|conn_sock| conn_sock.lock().state == SocketState::Connected)
            || s.closed
    });

    let new_sock_arc = {
        let mut s = sock_arc.lock();
        if s.closed {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
        // Find first Established entry
        let pos = s
            .accept_queue
            .iter()
            .position(|conn_sock| conn_sock.lock().state == SocketState::Connected);
        match pos {
            Some(i) => s.accept_queue.remove(i).unwrap(),
            None => {
                info.lock().errno = Errno::EAGAIN;
                return !0u64;
            }
        }
    };

    // Write remote address to caller if requested
    if !addr_ptr.is_null() {
        let remote_addr = new_sock_arc.lock().remote_addr;
        if let Some(remote) = remote_addr {
            let sockaddr = SockAddrIn {
                family: AF_INET as u16,
                port: remote.port.to_be(),
                addr: remote.ip,
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
                    try_write_user(addr_len_ptr, core::mem::size_of::<SockAddrIn>() as u32)
                } {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            }
        }
    }

    // Allocate a new fd for the connected socket
    let new_fd = fd_table
        .lock()
        .allocate_fd(FileDescriptor::Socket(new_sock_arc));
    new_fd as u64
}
