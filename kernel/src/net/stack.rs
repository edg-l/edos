use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::time::Duration;
use spin::Mutex;

use crate::{drivers::e1000e::E1000e, log, thread::waitqueue::WaitQueue, timer::Instant};

use super::arp::ArpCache;
use super::tcp::TcpConnection;
use super::{arp, ethernet, icmp, ipv4, socket, tcp, udp};
use socket::SocketAddr;

pub static NET_STACK: spin::Once<Mutex<NetStack>> = spin::Once::new();

pub fn net_stack() -> &'static Mutex<NetStack> {
    NET_STACK.get().expect("net stack not initialized")
}

pub struct PingWaiter {
    pub wq: Arc<WaitQueue>,
    pub rtt_us: Option<u64>,
    pub sent_at: Instant,
}

pub struct NetStack {
    pub nic: E1000e,
    pub local_ip: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub gateway_ip: [u8; 4],
    pub arp_cache: ArpCache,
    /// Keyed by (id, seq).
    pub ping_waiters: BTreeMap<(u16, u16), PingWaiter>,
    /// Keyed by (local_addr, remote_addr).
    pub tcp_connections: BTreeMap<(SocketAddr, SocketAddr), Arc<Mutex<TcpConnection>>>,
    ip_id_counter: u16,
}

impl NetStack {
    pub fn new(nic: E1000e) -> Self {
        Self {
            nic,
            local_ip: [10, 0, 2, 15],
            subnet_mask: [255, 255, 255, 0],
            gateway_ip: [10, 0, 2, 2],
            arp_cache: ArpCache::new(),
            ping_waiters: BTreeMap::new(),
            tcp_connections: BTreeMap::new(),
            ip_id_counter: 0,
        }
    }

    /// Called from the e1000e driver thread on each received packet.
    pub fn handle_rx(&mut self, data: &[u8]) {
        let Some((eth_hdr, payload)) = ethernet::parse_frame(data) else {
            return;
        };

        match eth_hdr.ethertype {
            x if x == ethernet::EtherType::Arp as u16 => self.handle_arp(payload),
            x if x == ethernet::EtherType::Ipv4 as u16 => self.handle_ipv4(payload),
            _ => {}
        }
    }

    fn handle_arp(&mut self, data: &[u8]) {
        let Some(pkt) = arp::ArpPacket::parse(data) else {
            return;
        };

        // Update ARP cache for the sender regardless of operation.
        self.arp_cache.insert(pkt.spa, pkt.sha);

        if pkt.oper == arp::ARP_REQUEST && pkt.tpa == self.local_ip {
            self.send_arp_reply(pkt.sha, pkt.spa);
        }
    }

    fn handle_ipv4(&mut self, data: &[u8]) {
        let Some((ip_hdr, payload)) = ipv4::parse(data) else {
            return;
        };

        // Only process packets addressed to us.
        if ip_hdr.dst_addr != self.local_ip {
            return;
        }

        if ip_hdr.protocol == ipv4::IpProtocol::Icmp as u8 {
            self.handle_icmp(&ip_hdr, payload);
        } else if ip_hdr.protocol == ipv4::IpProtocol::Udp as u8 {
            self.handle_udp(&ip_hdr, payload);
        } else if ip_hdr.protocol == ipv4::IpProtocol::Tcp as u8 {
            self.handle_tcp(&ip_hdr, payload);
        }
    }

    fn handle_udp(&mut self, ip_hdr: &ipv4::Ipv4Header, data: &[u8]) {
        let Some((udp_hdr, payload)) = udp::parse(data) else {
            return;
        };

        let port_table = socket::port_table().lock();
        if let Some(sock) = port_table.get(&(17, udp_hdr.dst_port)) {
            let mut s = sock.lock();
            if !s.closed {
                let src = socket::SocketAddr {
                    ip: ip_hdr.src_addr,
                    port: udp_hdr.src_port,
                };
                s.rx_queue.push_back((payload.to_vec(), src));
                s.rx_wq.wake_one();
            }
        }
    }

    fn handle_tcp(&mut self, ip_hdr: &ipv4::Ipv4Header, payload: &[u8]) {
        let Some((tcp_hdr, tcp_payload)) = tcp::parse(payload, ip_hdr.src_addr, ip_hdr.dst_addr)
        else {
            return;
        };

        let local = SocketAddr {
            ip: ip_hdr.dst_addr,
            port: tcp_hdr.dst_port,
        };
        let remote = SocketAddr {
            ip: ip_hdr.src_addr,
            port: tcp_hdr.src_port,
        };

        if let Some(conn) = self.tcp_connections.get(&(local, remote)).cloned() {
            let responses = conn.lock().handle_segment(&tcp_hdr, tcp_payload, payload);
            for seg in responses {
                let _ = self.send_ip(remote.ip, ipv4::IpProtocol::Tcp, &seg);
            }
        } else {
            // No connection found. If it's a SYN to a listening socket, handle that in Phase 4b.
            // Send RST for unexpected segments (unless it's a RST itself).
            if tcp_hdr.flags & tcp::RST == 0 {
                let rst = tcp::build(
                    tcp_hdr.dst_port,
                    tcp_hdr.src_port,
                    tcp_hdr.ack_num,
                    tcp_hdr.seq_num.wrapping_add(1),
                    tcp::RST | tcp::ACK,
                    0,
                    ip_hdr.dst_addr,
                    ip_hdr.src_addr,
                    &[],
                    &[],
                );
                let _ = self.send_ip(ip_hdr.src_addr, ipv4::IpProtocol::Tcp, &rst);
            }
        }
    }

    fn handle_icmp(&mut self, ip_hdr: &ipv4::Ipv4Header, data: &[u8]) {
        let Some((icmp_hdr, payload)) = icmp::parse(data) else {
            return;
        };

        match icmp_hdr.type_ {
            icmp::ECHO_REQUEST => {
                let reply = icmp::build_echo_reply(icmp_hdr.id, icmp_hdr.seq, payload);
                let _ = self.send_ip(ip_hdr.src_addr, ipv4::IpProtocol::Icmp, &reply);
            }
            icmp::ECHO_REPLY => {
                let key = (icmp_hdr.id, icmp_hdr.seq);
                if let Some(waiter) = self.ping_waiters.get_mut(&key) {
                    let rtt_us = waiter.sent_at.elapsed().as_micros() as u64;
                    waiter.rtt_us = Some(rtt_us);
                    waiter.wq.wake_one();
                }
            }
            _ => {}
        }
    }

    /// Send an IP packet, resolving the destination MAC via ARP.
    ///
    /// Returns `Err("arp pending")` on a cache miss after sending an ARP request.
    /// Callers that can block should retry after waiting on the ARP cache waiter.
    pub fn send_ip(
        &mut self,
        dst_ip: [u8; 4],
        protocol: ipv4::IpProtocol,
        payload: &[u8],
    ) -> Result<(), &'static str> {
        let resolve_ip = if self.is_local_subnet(&dst_ip) {
            dst_ip
        } else {
            self.gateway_ip
        };

        if let Some(dst_mac) = self.arp_cache.lookup(&resolve_ip) {
            let ip_pkt = ipv4::build(self.local_ip, dst_ip, protocol, 64, payload);
            let frame =
                ethernet::build_frame(dst_mac, self.mac(), ethernet::EtherType::Ipv4, &ip_pkt);
            self.nic.transmit(&frame)
        } else {
            self.send_arp_request(resolve_ip);
            Err("arp pending")
        }
    }

    fn send_arp_request(&mut self, target_ip: [u8; 4]) {
        let pkt = arp::ArpPacket {
            htype: 1,      // Ethernet
            ptype: 0x0800, // IPv4
            hlen: 6,
            plen: 4,
            oper: arp::ARP_REQUEST,
            sha: self.mac(),
            spa: self.local_ip,
            tha: [0u8; 6],
            tpa: target_ip,
        };
        let payload = pkt.serialize();
        let frame = ethernet::build_frame(
            ethernet::BROADCAST,
            self.mac(),
            ethernet::EtherType::Arp,
            &payload,
        );
        let _ = self.nic.transmit(&frame);
    }

    fn send_arp_reply(&mut self, target_mac: [u8; 6], target_ip: [u8; 4]) {
        let pkt = arp::ArpPacket {
            htype: 1,
            ptype: 0x0800,
            hlen: 6,
            plen: 4,
            oper: arp::ARP_REPLY,
            sha: self.mac(),
            spa: self.local_ip,
            tha: target_mac,
            tpa: target_ip,
        };
        let payload = pkt.serialize();
        let frame =
            ethernet::build_frame(target_mac, self.mac(), ethernet::EtherType::Arp, &payload);
        let _ = self.nic.transmit(&frame);
    }

    fn is_local_subnet(&self, ip: &[u8; 4]) -> bool {
        for i in 0..4 {
            if (ip[i] & self.subnet_mask[i]) != (self.local_ip[i] & self.subnet_mask[i]) {
                return false;
            }
        }
        true
    }

    pub fn mac(&self) -> [u8; 6] {
        self.nic.mac_addr
    }

    fn next_ip_id(&mut self) -> u16 {
        let id = self.ip_id_counter;
        self.ip_id_counter = self.ip_id_counter.wrapping_add(1);
        id
    }

    /// Send a UDP datagram to `dst_ip:dst_port` from `src_port`.
    pub fn send_udp(
        &mut self,
        src_port: u16,
        dst_ip: [u8; 4],
        dst_port: u16,
        data: &[u8],
    ) -> Result<(), &'static str> {
        let udp_pkt = udp::build(src_port, dst_port, self.local_ip, dst_ip, data);
        self.send_ip(dst_ip, ipv4::IpProtocol::Udp, &udp_pkt)
    }

    /// Send an ICMP echo request to `dst`. The caller must register a
    /// `PingWaiter` in `ping_waiters` before calling this if it wants to
    /// receive the reply notification.
    pub fn send_ping(&mut self, dst: [u8; 4], id: u16, seq: u16) -> Result<(), &'static str> {
        let _ = self.next_ip_id();
        let icmp_data = icmp::build_echo_request(id, seq, &[0u8; 32]);
        self.send_ip(dst, ipv4::IpProtocol::Icmp, &icmp_data)
    }
}

/// Kernel thread that periodically checks TCP connections for retransmit timeouts
/// and cleans up TIME_WAIT / Closed connections.
pub extern "C" fn tcp_retransmit_main() -> ! {
    use crate::thread::scheduler::sched;

    let thread = sched().current_thread().unwrap();
    thread.set_priority(crate::thread::runqueue::IO_PRIORITY);

    loop {
        sched().thread_sleep(Duration::from_millis(200));

        let Some(stack_mutex) = NET_STACK.get() else {
            continue;
        };

        // Collect connections to check (clone Arcs, not the connections themselves).
        let connections: Vec<_> = {
            let stack = stack_mutex.lock();
            stack.tcp_connections.values().cloned().collect()
        };

        for conn in connections {
            let resends = conn.lock().check_retransmit();
            if !resends.is_empty() {
                let remote_ip = conn.lock().remote_ip;
                let mut stack = stack_mutex.lock();
                for seg in resends {
                    let _ = stack.send_ip(remote_ip, super::ipv4::IpProtocol::Tcp, &seg);
                }
            }
        }

        // Clean up TIME_WAIT and Closed connections.
        {
            let mut stack = stack_mutex.lock();
            let now = Instant::now();
            stack.tcp_connections.retain(|_, conn| {
                let c = conn.lock();
                if c.state == super::tcp::TcpState::TimeWait {
                    if let Some(until) = c.time_wait_until {
                        return now < until;
                    }
                }
                c.state != super::tcp::TcpState::Closed
            });
        }
    }
}

/// Perform a ping from a syscall context.
///
/// Sends an ICMP echo request to `dst_ip` and blocks until a reply arrives or
/// `timeout` elapses.  Returns the round-trip time in microseconds, or `None`
/// on timeout / ARP failure.
pub fn syscall_ping(dst_ip: [u8; 4], id: u16, seq: u16, timeout: Duration) -> Option<u64> {
    use crate::thread::scheduler::sched;

    let wq = Arc::new(WaitQueue::new());
    let sent_at = Instant::now();

    // Register waiter before sending so the reply can never arrive before we
    // are ready to receive it.
    {
        let mut stack = net_stack().lock();
        stack.ping_waiters.insert(
            (id, seq),
            PingWaiter {
                wq: Arc::clone(&wq),
                rtt_us: None,
                sent_at,
            },
        );
    }

    // Send the ping, retrying up to 3 times if ARP resolution is pending.
    let mut sent = false;
    for attempt in 0..3u32 {
        let result = net_stack().lock().send_ping(dst_ip, id, seq);
        match result {
            Ok(()) => {
                sent = true;
                break;
            }
            Err("arp pending") => {
                // Get the ARP waiter while still holding the lock, then drop
                // it before sleeping.
                let arp_wq = {
                    let mut stack = net_stack().lock();
                    let resolve_ip = if stack.is_local_subnet(&dst_ip) {
                        dst_ip
                    } else {
                        stack.gateway_ip
                    };
                    stack.arp_cache.get_or_create_waiter(resolve_ip)
                };

                // Wait up to 100 ms for the ARP reply.
                let sleep_dur = Duration::from_millis(100);
                arp_wq.wait_until_timeout(
                    || {
                        let stack = net_stack().lock();
                        let resolve_ip = if stack.is_local_subnet(&dst_ip) {
                            dst_ip
                        } else {
                            stack.gateway_ip
                        };
                        stack.arp_cache.lookup(&resolve_ip).is_some()
                    },
                    Some(sleep_dur),
                );

                log!("net: ping ARP attempt {}", attempt + 1);
            }
            Err(e) => {
                log!("net: ping send_ip failed: {}", e);
                break;
            }
        }
    }

    if !sent {
        // Clean up waiter and return failure.
        net_stack().lock().ping_waiters.remove(&(id, seq));
        return None;
    }

    // Wait for the echo reply with a timeout.
    let key = (id, seq);
    wq.wait_until_timeout(
        || {
            net_stack()
                .lock()
                .ping_waiters
                .get(&key)
                .and_then(|w| w.rtt_us)
                .is_some()
        },
        Some(timeout),
    );

    let rtt = net_stack()
        .lock()
        .ping_waiters
        .remove(&key)
        .and_then(|w| w.rtt_us);

    rtt
}
