use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use crate::thread::waitqueue::WaitQueue;

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
            sha: <[u8; 6]>::try_from(&data[8..14]).unwrap(),
            spa: <[u8; 4]>::try_from(&data[14..18]).unwrap(),
            tha: <[u8; 6]>::try_from(&data[18..24]).unwrap(),
            tpa: <[u8; 4]>::try_from(&data[24..28]).unwrap(),
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

pub struct ArpCache {
    entries: BTreeMap<[u8; 4], [u8; 6]>,
    /// Waiters blocked on pending ARP resolution, keyed by target IP.
    pending: BTreeMap<[u8; 4], Arc<WaitQueue>>,
}

impl ArpCache {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }

    pub fn lookup(&self, ip: &[u8; 4]) -> Option<[u8; 6]> {
        self.entries.get(ip).copied()
    }

    pub fn insert(&mut self, ip: [u8; 4], mac: [u8; 6]) {
        const MAX_ARP_ENTRIES: usize = 256;
        // Evict oldest entry if cache is full.
        if self.entries.len() >= MAX_ARP_ENTRIES && !self.entries.contains_key(&ip) {
            if let Some(&oldest_ip) = self.entries.keys().next() {
                self.entries.remove(&oldest_ip);
            }
        }
        self.entries.insert(ip, mac);
        // Wake anyone waiting for this IP.
        if let Some(wq) = self.pending.remove(&ip) {
            wq.wake_all();
        }
    }

    pub fn get_or_create_waiter(&mut self, ip: [u8; 4]) -> Arc<WaitQueue> {
        self.pending
            .entry(ip)
            .or_insert_with(|| Arc::new(WaitQueue::new()))
            .clone()
    }
}
