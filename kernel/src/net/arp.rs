use alloc::{collections::BTreeMap, vec::Vec};
use core::time::Duration;

use crate::timer::Instant;

pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;
/// Packet length for IPv4 over Ethernet ARP.
pub const PACKET_LEN: usize = 28;

#[derive(Debug, Clone)]
pub struct ArpPacket {
    pub htype: u16,
    pub ptype: u16,
    pub hlen: u8,
    pub plen: u8,
    pub oper: u16,
    pub sha: [u8; 6], // sender hw addr
    pub spa: [u8; 4], // sender protocol addr
    pub tha: [u8; 6], // target hw addr
    pub tpa: [u8; 4], // target protocol addr
}

impl ArpPacket {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < PACKET_LEN {
            return None;
        }
        Some(Self {
            htype: u16::from_be_bytes([data[0], data[1]]),
            ptype: u16::from_be_bytes([data[2], data[3]]),
            hlen: data[4],
            plen: data[5],
            oper: u16::from_be_bytes([data[6], data[7]]),
            sha: data[8..14].try_into().ok()?,
            spa: data[14..18].try_into().ok()?,
            tha: data[18..24].try_into().ok()?,
            tpa: data[24..28].try_into().ok()?,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(PACKET_LEN);
        buf.extend_from_slice(&self.htype.to_be_bytes());
        buf.extend_from_slice(&self.ptype.to_be_bytes());
        buf.push(self.hlen);
        buf.push(self.plen);
        buf.extend_from_slice(&self.oper.to_be_bytes());
        buf.extend_from_slice(&self.sha);
        buf.extend_from_slice(&self.spa);
        buf.extend_from_slice(&self.tha);
        buf.extend_from_slice(&self.tpa);
        buf
    }
}

/// How long a packet waits for its target to answer. Past this the request is
/// abandoned rather than transmitted: a datagram released minutes late is worse
/// than one that was dropped, and a stream's own retransmit already covers the
/// loss.
const PENDING_TX_TTL: Duration = Duration::from_secs(3);

/// A packet held against an unresolved address, with when it was queued.
struct PendingTx {
    packet: Vec<u8>,
    queued_at: Instant,
}

pub struct ArpCache {
    entries: BTreeMap<[u8; 4], [u8; 6]>,
    /// One outbound IPv4 packet held per unresolved target, transmitted once
    /// the reply lands. RFC 1122 §2.3.2.2 requires an implementation to queue
    /// at least one packet rather than dropping it.
    pending_tx: BTreeMap<[u8; 4], PendingTx>,
}

impl ArpCache {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            pending_tx: BTreeMap::new(),
        }
    }

    pub fn lookup(&self, ip: &[u8; 4]) -> Option<[u8; 6]> {
        self.entries.get(ip).copied()
    }

    pub fn insert(&mut self, ip: [u8; 4], mac: [u8; 6]) {
        const MAX_ARP_ENTRIES: usize = 256;
        // A full cache drops its lowest address to make room. Entries carry no
        // age, so this is not an LRU and not an RFC 1122 §2.3.2.1 timeout; it
        // only bounds the map.
        if self.entries.len() >= MAX_ARP_ENTRIES && !self.entries.contains_key(&ip) {
            if let Some(&evicted) = self.entries.keys().next() {
                self.entries.remove(&evicted);
            }
        }
        self.entries.insert(ip, mac);
    }

    /// Hold `packet` until `ip` resolves. Newest wins: a second packet for the
    /// same target replaces the first, so a sender that keeps retrying cannot
    /// grow the queue.
    pub fn queue_pending_tx(&mut self, ip: [u8; 4], packet: Vec<u8>) {
        const MAX_PENDING_TX: usize = 16;
        let now = Instant::now();
        self.pending_tx
            .retain(|_, held| now.duration_since(held.queued_at) < PENDING_TX_TTL);
        if self.pending_tx.len() >= MAX_PENDING_TX && !self.pending_tx.contains_key(&ip) {
            // By age, not by address: the map is keyed by IP, so taking the
            // first key would evict the numerically lowest target every time.
            if let Some(oldest_ip) = self
                .pending_tx
                .iter()
                .min_by_key(|(_, held)| held.queued_at.as_nanos())
                .map(|(&ip, _)| ip)
            {
                self.pending_tx.remove(&oldest_ip);
            }
        }
        self.pending_tx.insert(
            ip,
            PendingTx {
                packet,
                queued_at: now,
            },
        );
    }

    /// Take the packet held for `ip`, if any, unless it has waited longer than
    /// [`PENDING_TX_TTL`]. A target that answers an hour later must not put a
    /// stale datagram on the wire.
    pub fn take_pending_tx(&mut self, ip: &[u8; 4]) -> Option<Vec<u8>> {
        let held = self.pending_tx.remove(ip)?;
        (held.queued_at.elapsed() < PENDING_TX_TTL).then_some(held.packet)
    }
}
