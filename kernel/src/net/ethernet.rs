use alloc::vec::Vec;

pub const HEADER_LEN: usize = 14;
pub const BROADCAST: [u8; 6] = [0xff; 6];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum EtherType {
    Ipv4 = 0x0800,
    Arp = 0x0806,
}

#[allow(dead_code)]
pub struct EthernetHeader {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    pub ethertype: u16,
}

pub fn parse_frame(data: &[u8]) -> Option<(EthernetHeader, &[u8])> {
    if data.len() < HEADER_LEN {
        return None;
    }
    let dst = <[u8; 6]>::try_from(&data[0..6]).ok()?;
    let src = <[u8; 6]>::try_from(&data[6..12]).ok()?;
    let ethertype = u16::from_be_bytes([data[12], data[13]]);
    Some((
        EthernetHeader {
            dst,
            src,
            ethertype,
        },
        &data[HEADER_LEN..],
    ))
}

pub fn build_frame(dst: [u8; 6], src: [u8; 6], ethertype: EtherType, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&dst);
    frame.extend_from_slice(&src);
    frame.extend_from_slice(&(ethertype as u16).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}
