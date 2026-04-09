use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};
use spin::Mutex;

use crate::net::tcp::TcpConnection;
use crate::thread::waitqueue::WaitQueue;

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
        }
    }
}

/// Global port table: maps (protocol, port) -> bound socket reference.
/// For UDP, protocol=17. For TCP (future), protocol=6.
pub static PORT_TABLE: spin::Once<Mutex<BTreeMap<(u8, u16), Arc<Mutex<Socket>>>>> =
    spin::Once::new();

pub fn port_table() -> &'static Mutex<BTreeMap<(u8, u16), Arc<Mutex<Socket>>>> {
    PORT_TABLE.call_once(|| Mutex::new(BTreeMap::new()))
}

/// Ephemeral port counter.
static EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);

const EPHEMERAL_START: u16 = 49152;
const EPHEMERAL_RANGE: u16 = 65535 - EPHEMERAL_START + 1; // 16384

pub fn allocate_ephemeral_port(protocol: u8) -> Option<u16> {
    let table = port_table().lock();
    for _ in 0..1000 {
        let raw = EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
        let port = EPHEMERAL_START + (raw % EPHEMERAL_RANGE);
        if !table.contains_key(&(protocol, port)) {
            return Some(port);
        }
    }
    None
}
