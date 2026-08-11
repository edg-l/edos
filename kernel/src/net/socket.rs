use crate::thread::preempt::PreemptSpinlock as Mutex;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};

use crate::debug::lock_order::{RANK_PORT_TABLE, RANK_SOCKET, RANK_TCP_CONN};
use crate::fs::PollState;
use crate::fs::handle::{PollEntry, PollKey, PollRegistration, Pollable};
use crate::net::tcp::{TcpConnection, TcpState};
use crate::thread::waitqueue::WaitQueue;
use crate::{ranked_lock, ranked_lock_same};

pub const AF_INET: u32 = 2;
pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SocketAddr {
    pub ip: [u8; 4],
    pub port: u16,
}

/// C-compatible sockaddr_in layout for syscall interface.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrIn {
    pub family: u16,
    pub port: u16, // network byte order (big-endian)
    pub addr: [u8; 4],
    pub zero: [u8; 8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(dead_code)]
pub enum SocketState {
    Unbound,
    Bound,
    Connected,
    Closed,
}

pub struct Socket {
    pub sock_type: u32, // SOCK_DGRAM or SOCK_STREAM
    pub state: SocketState,
    pub local_addr: Option<SocketAddr>,
    pub remote_addr: Option<SocketAddr>,
    /// Received datagrams: each entry is (data, source_addr). Used for UDP only.
    pub rx_queue: VecDeque<(Vec<u8>, SocketAddr)>,
    pub rx_wq: Arc<WaitQueue>,
    pub closed: bool,
    /// TCP connection state machine (only set for SOCK_STREAM).
    pub tcp_conn: Option<Arc<Mutex<TcpConnection>>>,
    /// Completed connections waiting for accept() (only for listening TCP sockets).
    pub accept_queue: VecDeque<Arc<Mutex<Socket>>>,
    /// Whether this socket is in listening state.
    pub listening: bool,
    /// Maximum accept queue length.
    pub backlog: u32,
    /// Number of fd references. Close only tears down when this reaches 0.
    pub refcount: u32,
    /// Registered poll entries for wakeup on state changes.
    pollers: Vec<(PollKey, Arc<PollEntry>)>,
    next_poll_key: PollKey,
    /// Receive timeout for blocking operations.
    pub recv_timeout: Option<core::time::Duration>,
    /// Send timeout for blocking operations.
    pub send_timeout: Option<core::time::Duration>,
    /// TCP_NODELAY option (disable Nagle's algorithm).
    pub nodelay: bool,
}

impl core::fmt::Debug for Socket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Socket")
            .field("sock_type", &self.sock_type)
            .field("state", &self.state)
            .field("local_addr", &self.local_addr)
            .field("remote_addr", &self.remote_addr)
            .field("closed", &self.closed)
            .field("listening", &self.listening)
            .field("backlog", &self.backlog)
            .finish_non_exhaustive()
    }
}

impl Socket {
    pub fn new_udp() -> Self {
        Self {
            sock_type: SOCK_DGRAM,
            state: SocketState::Unbound,
            local_addr: None,
            remote_addr: None,
            rx_queue: VecDeque::new(),
            rx_wq: Arc::new(WaitQueue::new()),
            closed: false,
            tcp_conn: None,
            accept_queue: VecDeque::new(),
            listening: false,
            backlog: 0,
            refcount: 1,
            pollers: Vec::new(),
            next_poll_key: 1,
            recv_timeout: None,
            send_timeout: None,
            nodelay: false,
        }
    }

    pub fn new_tcp() -> Self {
        Self {
            sock_type: SOCK_STREAM,
            state: SocketState::Unbound,
            local_addr: None,
            remote_addr: None,
            rx_queue: VecDeque::new(),
            rx_wq: Arc::new(WaitQueue::new()),
            closed: false,
            tcp_conn: None,
            accept_queue: VecDeque::new(),
            listening: false,
            backlog: 0,
            refcount: 1,
            pollers: Vec::new(),
            next_poll_key: 1,
            recv_timeout: None,
            send_timeout: None,
            nodelay: false,
        }
    }

    pub fn poll_state(&self) -> PollState {
        let mut state = PollState::none();
        if self.sock_type == SOCK_STREAM {
            if self.listening {
                // Listening socket: readable when accept_queue has a Connected entry
                if self.accept_queue.iter().any(|qs| {
                    ranked_lock_same!(RANK_SOCKET, "socket::poll_queued", qs).state
                        == SocketState::Connected
                }) {
                    state.readable = true;
                }
            } else if let Some(ref conn) = self.tcp_conn {
                let c = ranked_lock!(RANK_TCP_CONN, "socket::poll_conn", conn);
                if !c.rx_buffer.is_empty()
                    || c.state == TcpState::CloseWait
                    || c.state == TcpState::Closed
                    || c.state == TcpState::TimeWait
                {
                    state.readable = true;
                }
            }
        } else {
            // UDP
            if !self.rx_queue.is_empty() {
                state.readable = true;
            }
        }
        state.writable = true;
        if self.closed {
            state.hangup = true;
        }
        state
    }

    pub fn add_poller(&mut self, entry: Arc<PollEntry>) -> PollKey {
        let key = self.next_poll_key;
        self.next_poll_key = self.next_poll_key.wrapping_add(1).max(1);
        self.pollers.push((key, entry));
        key
    }

    pub fn remove_poller(&mut self, key: PollKey) {
        self.pollers.retain(|(stored, _)| *stored != key);
    }

    /// Snapshot poller entries + state. Caller must flush the result AFTER
    /// dropping all locks (NetStack, PORT_TABLE, Socket) to avoid priority inversion.
    pub fn notify_pollers(&mut self) -> SocketNotifications {
        let state = self.poll_state();
        if self.pollers.is_empty() {
            return SocketNotifications::EMPTY;
        }
        let entries: Vec<Arc<PollEntry>> = self
            .pollers
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect();
        SocketNotifications { entries, state }
    }
}

/// Deferred poll notifications to be flushed after releasing all locks.
pub struct SocketNotifications {
    entries: Vec<Arc<PollEntry>>,
    state: PollState,
}

impl SocketNotifications {
    pub const EMPTY: Self = Self {
        entries: Vec::new(),
        state: PollState::none(),
    };

    /// Send notifications. Call this after dropping all locks.
    pub fn flush(self) {
        for entry in &self.entries {
            entry.update(self.state);
        }
    }
}

#[derive(Debug, Clone)]
pub struct PollableSocket {
    inner: Arc<Mutex<Socket>>,
}

impl PollableSocket {
    pub fn new(sock: Arc<Mutex<Socket>>) -> Self {
        Self { inner: sock }
    }
}

impl Pollable for PollableSocket {
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration {
        let mut s = ranked_lock!(RANK_SOCKET, "socket::poll_register", self.inner);
        let state = s.poll_state();
        entry.update(state);
        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = s.add_poller(entry);
            PollRegistration {
                initial: state,
                key: Some(key),
            }
        }
    }

    fn unregister(&self, key: PollKey) {
        ranked_lock!(RANK_SOCKET, "socket::poll_unregister", self.inner).remove_poller(key);
    }
}

/// Global port table: maps (protocol, port) -> bound socket reference.
/// For UDP, protocol=17. For TCP (future), protocol=6.
pub static PORT_TABLE: spin::Once<Mutex<BTreeMap<(u8, u16), Arc<Mutex<Socket>>>>> =
    spin::Once::new();

pub fn port_table() -> &'static Mutex<BTreeMap<(u8, u16), Arc<Mutex<Socket>>>> {
    PORT_TABLE.call_once(|| Mutex::new(BTreeMap::new()))
}

/// The port-table key for a socket's bound address, if it has one.
pub fn port_key(sock: &Socket) -> Option<(u8, u16)> {
    let proto = if sock.sock_type == SOCK_DGRAM {
        17u8
    } else {
        6u8
    };
    sock.local_addr.map(|addr| (proto, addr.port))
}

/// Release a port-table entry on close, but only for the socket that owns it.
///
/// A socket returned by `accept` carries its listener's local port, and the
/// table maps a port to the one socket bound to it. Removing by port alone
/// unbinds the listener as soon as any of its connections closes, and the next
/// SYN is answered with RST.
///
/// The socket's own lock must not be held: the receive path takes the port
/// table before a socket, so the other order is an AB/BA against it.
pub fn unbind_port(sock: &Arc<Mutex<Socket>>, key: (u8, u16)) {
    let mut table = ranked_lock!(RANK_PORT_TABLE, "socket::unbind_port", port_table());
    if table
        .get(&key)
        .is_some_and(|owner| Arc::ptr_eq(owner, sock))
    {
        table.remove(&key);
    }
}

/// Ephemeral port counter.
static EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);

const EPHEMERAL_START: u16 = 49152;
const EPHEMERAL_RANGE: u16 = 65535 - EPHEMERAL_START + 1; // 16384

/// Allocate an ephemeral port and insert `sock` into the port table atomically.
/// Returns the allocated port, or None if all ports are in use.
pub fn allocate_ephemeral_port(protocol: u8, sock: Arc<Mutex<Socket>>) -> Option<u16> {
    let mut table = ranked_lock!(RANK_PORT_TABLE, "socket::alloc_port", port_table());
    for _ in 0..1000 {
        let raw = EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
        let port = EPHEMERAL_START + (raw % EPHEMERAL_RANGE);
        if !table.contains_key(&(protocol, port)) {
            table.insert((protocol, port), sock);
            return Some(port);
        }
    }
    None
}
