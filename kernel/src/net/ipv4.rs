use alloc::vec::Vec;

use super::checksum::internet_checksum;

/// Minimum IPv4 header length (no options).
pub const HEADER_LEN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IpProtocol {
    Icmp = 1,
    Tcp = 6,
    Udp = 17,
}

#[derive(Debug, Clone)]
#[expect(
    dead_code,
    reason = "the IPv4 header in full (RFC 791 §3.1); the stack reads a subset"
)]
pub struct Ipv4Header {
    pub version_ihl: u8,
    pub dscp_ecn: u8,
    pub total_length: u16,
    pub identification: u16,
    pub flags_fragment: u16,
    pub ttl: u8,
    pub protocol: u8,
    pub checksum: u16,
    pub src_addr: [u8; 4],
    pub dst_addr: [u8; 4],
}

impl Ipv4Header {
    pub fn header_len(&self) -> usize {
        ((self.version_ihl & 0x0F) as usize) * 4
    }
}

pub fn parse(data: &[u8]) -> Option<(Ipv4Header, &[u8])> {
    if data.len() < HEADER_LEN {
        return None;
    }
    let version = data[0] >> 4;
    if version != 4 {
        return None;
    }

    let hdr = Ipv4Header {
        version_ihl: data[0],
        dscp_ecn: data[1],
        total_length: u16::from_be_bytes([data[2], data[3]]),
        identification: u16::from_be_bytes([data[4], data[5]]),
        flags_fragment: u16::from_be_bytes([data[6], data[7]]),
        ttl: data[8],
        protocol: data[9],
        checksum: u16::from_be_bytes([data[10], data[11]]),
        src_addr: <[u8; 4]>::try_from(&data[12..16]).unwrap(),
        dst_addr: <[u8; 4]>::try_from(&data[16..20]).unwrap(),
    };

    let hdr_len = hdr.header_len();
    if data.len() < hdr_len {
        return None;
    }

    // Verify header checksum.
    if internet_checksum(&data[..hdr_len]) != 0 {
        return None;
    }

    let total = hdr.total_length as usize;
    if data.len() < total {
        return None;
    }

    Some((hdr, &data[hdr_len..total]))
}

/// Build an IPv4 packet with a valid header checksum.
pub fn build(
    src: [u8; 4],
    dst: [u8; 4],
    protocol: IpProtocol,
    ttl: u8,
    id: u16,
    payload: &[u8],
) -> Vec<u8> {
    let total_length = (HEADER_LEN + payload.len()) as u16;
    let mut pkt = Vec::with_capacity(total_length as usize);

    // Build header with zero checksum placeholder first.
    pkt.push(0x45); // version=4, ihl=5
    pkt.push(0x00); // dscp_ecn
    pkt.extend_from_slice(&total_length.to_be_bytes());
    pkt.extend_from_slice(&id.to_be_bytes());
    pkt.extend_from_slice(&0x4000u16.to_be_bytes()); // flags: DF=1, MF=0, offset=0
    pkt.push(ttl);
    pkt.push(protocol as u8);
    pkt.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    pkt.extend_from_slice(&src);
    pkt.extend_from_slice(&dst);

    // Compute and fill checksum.
    let cksum = internet_checksum(&pkt[..HEADER_LEN]);
    pkt[10] = (cksum >> 8) as u8;
    pkt[11] = (cksum & 0xFF) as u8;

    pkt.extend_from_slice(payload);
    pkt
}
