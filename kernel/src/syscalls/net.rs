//! Network socket syscalls: socket, bind, connect, sendto, recvfrom.

use crate::debug::lock_order::{RANK_NET_STACK, RANK_PORT_TABLE, RANK_SOCKET, RANK_TCP_CONN};
use crate::thread::preempt::PreemptSpinlock as Mutex;
use crate::{ranked_lock, ranked_lock_same};
use alloc::sync::Arc;

use core::time::Duration;

use crate::drivers::hpet::instant::HpetInstant;
use crate::thread::scheduler::current_thread_info;
use crate::thread::waitqueue::WaitOutcome;
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
    thread::pipe::FileDescriptor,
    util::uaccess::{try_copy_to_user, try_read_user, try_write_user},
};

pub use crate::net::socket::SockAddrIn;

pub fn sys_socket(domain: u64, sock_type: u64, _protocol: u64) -> u64 {
    let info = current_thread_info();
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
    let info = current_thread_info();
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
        let s = ranked_lock!(RANK_SOCKET, "sys_bind", sock_arc);
        (s.closed, s.sock_type)
    };
    if closed {
        info.lock().errno = Errno::EBADF;
        return !0u64;
    }

    let proto = if sock_type == SOCK_DGRAM { 17u8 } else { 6u8 };

    // Auto-assign ephemeral port if port 0 is requested
    let bind_port = if port == 0 {
        match allocate_ephemeral_port(proto, sock_arc.clone()) {
            Some(p) => p,
            None => {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }
        }
    } else {
        // Explicit port: register in port table.
        let mut table = ranked_lock!(RANK_PORT_TABLE, "sys_bind", port_table());
        if table.contains_key(&(proto, port)) {
            info.lock().errno = Errno::EADDRINUSE;
            return !0u64;
        }
        table.insert((proto, port), sock_arc.clone());
        port
    };

    let mut s = ranked_lock!(RANK_SOCKET, "sys_bind", sock_arc);
    s.local_addr = Some(SocketAddr {
        ip: local_addr.ip,
        port: bind_port,
    });
    s.state = SocketState::Bound;
    0
}

pub fn sys_connect(fd: u64, addr_ptr: *const SockAddrIn, addr_len: u64) -> u64 {
    let info = current_thread_info();
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
        let s = ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc);
        if s.closed {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
        s.sock_type
    };

    if sock_type == SOCK_STREAM {
        // TCP active open: auto-bind if needed, build SYN, wait for Established
        let local_port = {
            let needs_bind =
                ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc).state == SocketState::Unbound;
            if needs_bind {
                match allocate_ephemeral_port(6u8, sock_arc.clone()) {
                    Some(ep) => {
                        let mut s = ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc);
                        s.local_addr = Some(SocketAddr {
                            ip: [0u8; 4],
                            port: ep,
                        });
                        s.state = SocketState::Bound;
                        ep
                    }
                    None => {
                        info.lock().errno = Errno::EINVAL;
                        return !0u64;
                    }
                }
            } else {
                match ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc).local_addr {
                    Some(a) => a.port,
                    None => {
                        info.lock().errno = Errno::EINVAL;
                        return !0u64;
                    }
                }
            }
        };

        let local_ip = ranked_lock!(RANK_NET_STACK, "sys_connect", net_stack()).local_ip;
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
            let mut stack = ranked_lock!(RANK_NET_STACK, "sys_connect", net_stack());
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
                    || {
                        ranked_lock!(RANK_NET_STACK, "sys_connect", net_stack())
                            .arp_cache
                            .lookup(&resolve_ip)
                            .is_some()
                    },
                    Some(Duration::from_millis(200)),
                );
                let _ = net_stack()
                    .lock()
                    .send_ip(ip, ipv4::IpProtocol::Tcp, &syn_seg);
            }
        }

        ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc).tcp_conn = Some(conn_arc.clone());
        ranked_lock!(RANK_TCP_CONN, "sys_connect", conn_arc).owner =
            Some(Arc::downgrade(&sock_arc));

        let state_wq = ranked_lock!(RANK_TCP_CONN, "sys_connect", conn_arc)
            .state_wq
            .clone();
        state_wq.wait_until_timeout(
            || {
                let c = ranked_lock!(RANK_TCP_CONN, "sys_connect", conn_arc);
                c.state == TcpState::Established || c.state == TcpState::Closed
            },
            Some(Duration::from_secs(5)),
        );

        let state = ranked_lock!(RANK_TCP_CONN, "sys_connect", conn_arc).state;
        if state == TcpState::Established {
            ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc).state = SocketState::Connected;
            0
        } else {
            // Connection failed, clean up
            net_stack()
                .lock()
                .tcp_connections
                .remove(&(local_sa, remote_sa));
            ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc).tcp_conn = None;
            info.lock().errno = Errno::ECONNREFUSED;
            !0u64
        }
    } else {
        // UDP connect: just set remote_addr
        let needs_bind =
            ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc).state == SocketState::Unbound;
        if needs_bind {
            match allocate_ephemeral_port(17u8, sock_arc.clone()) {
                Some(ep) => {
                    let mut s = ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc);
                    s.local_addr = Some(SocketAddr {
                        ip: [0u8; 4],
                        port: ep,
                    });
                    s.state = SocketState::Bound;
                }
                None => {
                    info.lock().errno = Errno::EINVAL;
                    return !0u64;
                }
            }
        }
        let mut s = ranked_lock!(RANK_SOCKET, "sys_connect", sock_arc);
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
    let info = current_thread_info();
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
        match ranked_lock!(RANK_SOCKET, "sys_sendto", sock_arc).remote_addr {
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
        let (closed, needs_bind, proto, existing_port) = {
            let s = ranked_lock!(RANK_SOCKET, "sys_sendto", sock_arc);
            let proto = if s.sock_type == SOCK_DGRAM { 17u8 } else { 6u8 };
            (
                s.closed,
                s.local_addr.is_none(),
                proto,
                s.local_addr.map(|a| a.port),
            )
        };
        if closed {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
        if needs_bind {
            match allocate_ephemeral_port(proto, sock_arc.clone()) {
                Some(ep) => {
                    let mut s = ranked_lock!(RANK_SOCKET, "sys_sendto", sock_arc);
                    s.local_addr = Some(SocketAddr {
                        ip: [0u8; 4],
                        port: ep,
                    });
                    s.state = SocketState::Bound;
                    ep
                }
                None => {
                    info.lock().errno = Errno::EINVAL;
                    return !0u64;
                }
            }
        } else {
            existing_port.unwrap()
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
                    let mut stack = ranked_lock!(RANK_NET_STACK, "sys_sendto", net_stack());
                    let resolve_ip = if stack.is_local_subnet(&dst.ip) {
                        dst.ip
                    } else {
                        stack.gateway_ip
                    };
                    stack.arp_cache.get_or_create_waiter(resolve_ip)
                };
                arp_wq.wait_until_timeout(
                    || {
                        let stack = ranked_lock!(RANK_NET_STACK, "sys_sendto", net_stack());
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
    let info = current_thread_info();
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

    // `wait_until` returns on any wake, not only when the condition holds: a
    // wake token left by an earlier wait aborts the park. Loop on the real
    // condition, or the first receive on a socket reports an empty datagram
    // the moment anything else has woken this thread. That is what made the
    // first DNS query after boot come back with zero bytes.
    let (rx_wq, timeout) = {
        let s = ranked_lock!(RANK_SOCKET, "sys_recvfrom", sock_arc);
        (s.rx_wq.clone(), s.recv_timeout)
    };
    let deadline = timeout.map(|timeout| HpetInstant::now() + timeout);
    loop {
        let ready = || {
            let s = ranked_lock!(RANK_SOCKET, "sys_recvfrom", sock_arc);
            !s.rx_queue.is_empty() || s.closed
        };
        if ready() {
            break;
        }
        // SO_RCVTIMEO, so a datagram that never arrives costs the caller its
        // timeout rather than the thread. The remaining time comes off the
        // deadline each round, since a spurious wake must not restart it.
        let remaining = match deadline {
            Some(deadline) => match deadline.checked_duration_since(HpetInstant::now()) {
                Some(remaining) => Some(remaining),
                None => {
                    info.lock().errno = Errno::EAGAIN;
                    return !0u64;
                }
            },
            None => None,
        };
        if rx_wq.wait_until_timeout(ready, remaining) == WaitOutcome::TimedOut {
            info.lock().errno = Errno::EAGAIN;
            return !0u64;
        }
    }

    let (data, src) = {
        let mut s = ranked_lock!(RANK_SOCKET, "sys_recvfrom", sock_arc);
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
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    let mut s = ranked_lock!(RANK_SOCKET, "sys_listen", sock_arc);

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
        let mut table = ranked_lock!(RANK_PORT_TABLE, "sys_listen", port_table());
        if !table.contains_key(&(6u8, local_addr.port)) {
            table.insert((6u8, local_addr.port), sock_arc.clone());
        }
    }

    s.listening = true;
    s.backlog = if backlog == 0 { 1 } else { backlog };
    0
}

pub fn sys_accept(fd: u64, addr_ptr: *mut SockAddrIn, addr_len_ptr: *mut u32) -> u64 {
    let info = current_thread_info();
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
        let s = ranked_lock!(RANK_SOCKET, "sys_accept", sock_arc);
        if s.sock_type != SOCK_STREAM || !s.listening {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
        if s.closed {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    }

    // Loop on the condition, per the contract `wait_until` documents: a wake
    // token left by an earlier wait aborts the park, and returning then would
    // hand the caller EAGAIN on a listener that is simply idle.
    let rx_wq = ranked_lock!(RANK_SOCKET, "sys_accept", sock_arc)
        .rx_wq
        .clone();
    loop {
        let ready = || {
            let s = ranked_lock!(RANK_SOCKET, "sys_accept", sock_arc);
            s.accept_queue.iter().any(|conn_sock| {
                ranked_lock_same!(RANK_SOCKET, "sys_accept", conn_sock).state
                    == SocketState::Connected
            }) || s.closed
        };
        if ready() {
            break;
        }
        rx_wq.wait_until(ready);
    }

    let new_sock_arc = {
        let mut s = ranked_lock!(RANK_SOCKET, "sys_accept", sock_arc);
        if s.closed {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
        // Find first Established entry
        let pos = s.accept_queue.iter().position(|conn_sock| {
            ranked_lock_same!(RANK_SOCKET, "sys_accept", conn_sock).state == SocketState::Connected
        });
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
        let remote_addr = ranked_lock!(RANK_SOCKET, "sys_accept", new_sock_arc).remote_addr;
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

/// Timeval struct matching the C layout for SO_RCVTIMEO/SO_SNDTIMEO.
#[repr(C)]
#[derive(Clone, Copy)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}

/// Linger struct matching the C layout for SO_LINGER.
#[repr(C)]
#[derive(Clone, Copy)]
struct LingerVal {
    l_onoff: i32,
    l_linger: i32,
}

// Socket option constants
const SOL_SOCKET: i32 = 1;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_IP: i32 = 0;
const SO_RCVTIMEO: i32 = 20;
const SO_SNDTIMEO: i32 = 21;
const SO_LINGER: i32 = 13;
const SO_ERROR: i32 = 4;
const SO_REUSEADDR: i32 = 2;
const SO_BROADCAST: i32 = 6;
const TCP_NODELAY: i32 = 1;
const IP_TTL: i32 = 2;

pub fn sys_shutdown(fd: u64, how: u64) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    let how = how as i32;
    if how < 0 || how > 2 {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    let s = ranked_lock!(RANK_SOCKET, "sys_shutdown", sock_arc);
    if s.closed {
        info.lock().errno = Errno::EBADF;
        return !0u64;
    }

    // For TCP, send FIN if shutting down write side
    if s.sock_type == SOCK_STREAM && (how == 1 || how == 2) {
        if let Some(ref conn) = s.tcp_conn {
            let fin = ranked_lock!(RANK_TCP_CONN, "sys_shutdown", conn).build_fin();
            if let Some(fin_seg) = fin {
                let remote_ip = ranked_lock!(RANK_TCP_CONN, "sys_shutdown", conn).remote_ip;
                drop(s);
                if let Some(stack_mutex) = crate::net::stack::NET_STACK.get() {
                    let mut stack = ranked_lock!(RANK_NET_STACK, "sys_shutdown", stack_mutex);
                    let _ = stack.send_ip(remote_ip, crate::net::ipv4::IpProtocol::Tcp, &fin_seg);
                }
                return 0;
            }
        }
    }

    0
}

pub fn sys_setsockopt(fd: u64, level: i32, optname: i32, val_ptr: *const u8, val_len: u32) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    match (level, optname) {
        (SOL_SOCKET, SO_RCVTIMEO) | (SOL_SOCKET, SO_SNDTIMEO) => {
            if val_len < core::mem::size_of::<Timeval>() as u32 {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }
            let tv: Timeval = match unsafe { try_read_user(val_ptr as *const Timeval) } {
                Some(v) => v,
                None => {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            };
            let dur = if tv.tv_sec == 0 && tv.tv_usec == 0 {
                None
            } else {
                Some(Duration::new(tv.tv_sec as u64, (tv.tv_usec as u32) * 1000))
            };
            let mut s = ranked_lock!(RANK_SOCKET, "sys_setsockopt", sock_arc);
            if optname == SO_RCVTIMEO {
                s.recv_timeout = dur;
            } else {
                s.send_timeout = dur;
            }
            0
        }
        (SOL_SOCKET, SO_LINGER) => {
            if val_len < core::mem::size_of::<LingerVal>() as u32 {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }
            // Accept but don't implement linger behavior
            0
        }
        (IPPROTO_TCP, TCP_NODELAY) => {
            if val_len < 4 {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }
            let val: i32 = match unsafe { try_read_user(val_ptr as *const i32) } {
                Some(v) => v,
                None => {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            };
            ranked_lock!(RANK_SOCKET, "sys_setsockopt", sock_arc).nodelay = val != 0;
            0
        }
        // Accept SO_REUSEADDR and SO_BROADCAST silently (no-op)
        (SOL_SOCKET, SO_REUSEADDR) | (SOL_SOCKET, SO_BROADCAST) => 0,
        // Accept IP_TTL silently (no-op)
        (IPPROTO_IP, IP_TTL) => 0,
        _ => {
            // Unknown option: return success to not break callers
            0
        }
    }
}

pub fn sys_getsockopt(
    fd: u64,
    level: i32,
    optname: i32,
    val_ptr: *mut u8,
    val_len_ptr: *mut u32,
) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    match (level, optname) {
        (SOL_SOCKET, SO_RCVTIMEO) | (SOL_SOCKET, SO_SNDTIMEO) => {
            let s = ranked_lock!(RANK_SOCKET, "sys_getsockopt", sock_arc);
            let dur = if optname == SO_RCVTIMEO {
                s.recv_timeout
            } else {
                s.send_timeout
            };
            let tv = match dur {
                Some(d) => Timeval {
                    tv_sec: d.as_secs() as i64,
                    tv_usec: d.subsec_micros() as i64,
                },
                None => Timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
            };
            drop(s);
            if !unsafe { try_write_user(val_ptr as *mut Timeval, tv) } {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            }
            if !val_len_ptr.is_null() {
                if !unsafe { try_write_user(val_len_ptr, core::mem::size_of::<Timeval>() as u32) } {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            }
            0
        }
        (SOL_SOCKET, SO_LINGER) => {
            let linger = LingerVal {
                l_onoff: 0,
                l_linger: 0,
            };
            if !unsafe { try_write_user(val_ptr as *mut LingerVal, linger) } {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            }
            if !val_len_ptr.is_null() {
                if !unsafe { try_write_user(val_len_ptr, core::mem::size_of::<LingerVal>() as u32) }
                {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            }
            0
        }
        (IPPROTO_TCP, TCP_NODELAY) => {
            let val: i32 = if ranked_lock!(RANK_SOCKET, "sys_getsockopt", sock_arc).nodelay {
                1
            } else {
                0
            };
            if !unsafe { try_write_user(val_ptr as *mut i32, val) } {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            }
            if !val_len_ptr.is_null() {
                if !unsafe { try_write_user(val_len_ptr, 4u32) } {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            }
            0
        }
        (SOL_SOCKET, SO_ERROR) => {
            // Always return 0 (no pending error)
            let val: i32 = 0;
            if !unsafe { try_write_user(val_ptr as *mut i32, val) } {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            }
            if !val_len_ptr.is_null() {
                if !unsafe { try_write_user(val_len_ptr, 4u32) } {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            }
            0
        }
        _ => {
            // Unknown: return 0 as value
            let val: i32 = 0;
            if !unsafe { try_write_user(val_ptr as *mut i32, val) } {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            }
            if !val_len_ptr.is_null() {
                if !unsafe { try_write_user(val_len_ptr, 4u32) } {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            }
            0
        }
    }
}

pub fn sys_getpeername(fd: u64, addr_ptr: *mut SockAddrIn, addr_len_ptr: *mut u32) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    let remote = ranked_lock!(RANK_SOCKET, "sys_getpeername", sock_arc).remote_addr;
    match remote {
        Some(addr) => {
            let sockaddr = SockAddrIn {
                family: AF_INET as u16,
                port: addr.port.to_be(),
                addr: addr.ip,
                zero: [0u8; 8],
            };
            if !unsafe { try_write_user(addr_ptr, sockaddr) } {
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
            0
        }
        None => {
            info.lock().errno = Errno::ENOTCONN;
            !0u64
        }
    }
}

pub fn sys_getsockname(fd: u64, addr_ptr: *mut SockAddrIn, addr_len_ptr: *mut u32) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let sock_arc = match fd_table.lock().get_fd(fd).cloned() {
        Some(FileDescriptor::Socket(s)) => s,
        _ => {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
    };

    let local = ranked_lock!(RANK_SOCKET, "sys_getsockname", sock_arc).local_addr;
    match local {
        Some(addr) => {
            let sockaddr = SockAddrIn {
                family: AF_INET as u16,
                port: addr.port.to_be(),
                addr: addr.ip,
                zero: [0u8; 8],
            };
            if !unsafe { try_write_user(addr_ptr, sockaddr) } {
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
            0
        }
        None => {
            // Unbound socket: return zeroed address
            let sockaddr = SockAddrIn {
                family: AF_INET as u16,
                port: 0,
                addr: [0; 4],
                zero: [0u8; 8],
            };
            if !unsafe { try_write_user(addr_ptr, sockaddr) } {
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
            0
        }
    }
}

/// Write the resolver address into a caller-supplied `[u8; 4]`.
///
/// A resolver is configuration, not a socket operation, and there is no
/// filesystem convention for it here the way `/etc/resolv.conf` serves Unix,
/// so userspace asks the stack that learned it from DHCP.
pub fn sys_getdns(addr_ptr: *mut [u8; 4]) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if addr_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let Some(stack) = crate::net::stack::NET_STACK.get() else {
        info.lock().errno = Errno::ENOTCONN;
        return !0u64;
    };
    let dns = stack.lock().dns_server;

    if !unsafe { try_write_user(addr_ptr, dns) } {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }
    0
}
