use alloc::vec::Vec;

use super::checksum::internet_checksum;

pub const ECHO_REQUEST: u8 = 8;
pub const ECHO_REPLY: u8 = 0;

#[derive(Debug, Clone)]
#[expect(dead_code)]
pub struct IcmpHeader {
    pub type_: u8,
    pub code: u8,
    pub checksum: u16,
    pub id: u16,
    pub seq: u16,
}

pub fn parse(data: &[u8]) -> Option<(IcmpHeader, &[u8])> {
    if data.len() < 8 {
        return None;
    }
    // Verify checksum over entire ICMP message.
    if internet_checksum(data) != 0 {
        return None;
    }
    Some((
        IcmpHeader {
            type_: data[0],
            code: data[1],
            checksum: u16::from_be_bytes([data[2], data[3]]),
            id: u16::from_be_bytes([data[4], data[5]]),
            seq: u16::from_be_bytes([data[6], data[7]]),
        },
        &data[8..],
    ))
}

pub fn build_echo_request(id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    build_icmp(ECHO_REQUEST, 0, id, seq, payload)
}

pub fn build_echo_reply(id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    build_icmp(ECHO_REPLY, 0, id, seq, payload)
}

fn build_icmp(type_: u8, code: u8, id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8 + payload.len());
    msg.push(type_);
    msg.push(code);
    msg.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    msg.extend_from_slice(&id.to_be_bytes());
    msg.extend_from_slice(&seq.to_be_bytes());
    msg.extend_from_slice(payload);

    let cksum = internet_checksum(&msg);
    msg[2] = (cksum >> 8) as u8;
    msg[3] = (cksum & 0xFF) as u8;
    msg
}
