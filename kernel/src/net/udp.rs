use alloc::vec::Vec;

use super::checksum::internet_checksum;

pub const HEADER_LEN: usize = 8;

#[derive(Debug, Clone)]
pub struct UdpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub length: u16,
    pub checksum: u16,
}

/// Parse a datagram and verify its checksum against the addresses the IP layer
/// carried it under.
///
/// RFC 768 makes the checksum optional over IPv4 and reserves a transmitted
/// zero for "sender computed none", so only a non-zero field is checked. The
/// checksum covers a pseudo-header built from the source and destination
/// addresses, which is why they have to come from the caller.
pub fn parse(data: &[u8], src_ip: [u8; 4], dst_ip: [u8; 4]) -> Option<(UdpHeader, &[u8])> {
    if data.len() < HEADER_LEN {
        return None;
    }
    let hdr = UdpHeader {
        src_port: u16::from_be_bytes([data[0], data[1]]),
        dst_port: u16::from_be_bytes([data[2], data[3]]),
        length: u16::from_be_bytes([data[4], data[5]]),
        checksum: u16::from_be_bytes([data[6], data[7]]),
    };
    let payload_len = hdr.length as usize;
    if payload_len < HEADER_LEN || data.len() < payload_len {
        return None;
    }

    // Over the datagram alone: a frame padded to the Ethernet minimum carries
    // trailing bytes the sender never checksummed.
    let datagram = &data[..payload_len];
    if hdr.checksum != 0 && udp_checksum(datagram, src_ip, dst_ip) != 0 {
        return None;
    }

    Some((hdr, &datagram[HEADER_LEN..]))
}

pub fn build(
    src_port: u16,
    dst_port: u16,
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    payload: &[u8],
) -> Vec<u8> {
    let length = (HEADER_LEN + payload.len()) as u16;
    let mut pkt = Vec::with_capacity(length as usize);
    pkt.extend_from_slice(&src_port.to_be_bytes());
    pkt.extend_from_slice(&dst_port.to_be_bytes());
    pkt.extend_from_slice(&length.to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    pkt.extend_from_slice(payload);

    // UDP pseudo-header checksum. A computed zero goes on the wire as all ones,
    // since RFC 768 gives a transmitted zero the distinct meaning "no checksum".
    let cksum = match udp_checksum(&pkt, src_ip, dst_ip) {
        0 => 0xFFFF,
        c => c,
    };
    pkt[6] = (cksum >> 8) as u8;
    pkt[7] = (cksum & 0xFF) as u8;
    pkt
}

fn udp_checksum(udp_pkt: &[u8], src_ip: [u8; 4], dst_ip: [u8; 4]) -> u16 {
    let mut pseudo = Vec::with_capacity(12 + udp_pkt.len());
    pseudo.extend_from_slice(&src_ip);
    pseudo.extend_from_slice(&dst_ip);
    pseudo.push(0); // zero
    pseudo.push(17); // protocol UDP
    pseudo.extend_from_slice(&(udp_pkt.len() as u16).to_be_bytes());
    pseudo.extend_from_slice(udp_pkt);
    internet_checksum(&pseudo)
}
