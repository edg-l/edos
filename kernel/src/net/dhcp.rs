use alloc::{vec, vec::Vec};
use core::time::Duration;

use crate::{drivers::e1000e::E1000e, log, timer::Instant};

use super::{checksum::internet_checksum, ethernet};

const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];

// DHCP message types
const DHCPDISCOVER: u8 = 1;
const DHCPOFFER: u8 = 2;
const DHCPREQUEST: u8 = 3;
const DHCPACK: u8 = 5;

// DHCP options
const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_PARAM_LIST: u8 = 55;
const OPT_END: u8 = 255;

#[derive(Debug, Clone)]
pub struct DhcpLease {
    pub ip: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub gateway: [u8; 4],
    #[expect(dead_code)]
    pub dns: [u8; 4],
    pub server_id: [u8; 4],
}

/// Run the DHCP handshake using the NIC directly.
/// Returns a lease on success, None on timeout.
pub fn discover(nic: &mut E1000e, mac: [u8; 6]) -> Option<DhcpLease> {
    let xid: u32 = {
        let mut buf = [0u8; 4];
        crate::drivers::random::fill_bytes(&mut buf);
        u32::from_ne_bytes(buf)
    };

    // Step 1: Send DHCPDISCOVER
    let discover_pkt = build_dhcp_packet(DHCPDISCOVER, mac, xid, [0, 0, 0, 0], None, None);
    let frame = wrap_udp_broadcast(mac, &discover_pkt);
    log!("net: dhcp: sending DISCOVER");
    if nic.transmit(&frame).is_err() {
        log!("net: dhcp: failed to send DISCOVER");
        return None;
    }

    // Step 2: Wait for DHCPOFFER
    let offer = poll_for_dhcp(nic, xid, DHCPOFFER, Duration::from_secs(5))?;
    log!(
        "net: dhcp: received OFFER {}.{}.{}.{}",
        offer.ip[0],
        offer.ip[1],
        offer.ip[2],
        offer.ip[3]
    );

    // Step 3: Send DHCPREQUEST
    let request_pkt = build_dhcp_packet(
        DHCPREQUEST,
        mac,
        xid,
        [0, 0, 0, 0],
        Some(offer.ip),
        Some(offer.server_id),
    );
    let frame = wrap_udp_broadcast(mac, &request_pkt);
    log!(
        "net: dhcp: sending REQUEST for {}.{}.{}.{}",
        offer.ip[0],
        offer.ip[1],
        offer.ip[2],
        offer.ip[3]
    );
    if nic.transmit(&frame).is_err() {
        log!("net: dhcp: failed to send REQUEST");
        return None;
    }

    // Step 4: Wait for DHCPACK
    let ack = poll_for_dhcp(nic, xid, DHCPACK, Duration::from_secs(5))?;
    log!(
        "net: dhcp: received ACK, IP {}.{}.{}.{}",
        ack.ip[0],
        ack.ip[1],
        ack.ip[2],
        ack.ip[3]
    );

    Some(ack)
}

/// Build a DHCP message (BOOTP + options payload, no UDP/IP/Ethernet headers).
fn build_dhcp_packet(
    msg_type: u8,
    mac: [u8; 6],
    xid: u32,
    client_ip: [u8; 4],
    requested_ip: Option<[u8; 4]>,
    server_id: Option<[u8; 4]>,
) -> Vec<u8> {
    // Fixed BOOTP header (236 bytes) + 4 byte magic cookie
    let mut pkt = vec![0u8; 240];

    pkt[0] = 1; // op: BOOTREQUEST
    pkt[1] = 1; // htype: Ethernet
    pkt[2] = 6; // hlen: MAC length
    pkt[3] = 0; // hops
    pkt[4..8].copy_from_slice(&xid.to_be_bytes());
    // secs at [8..10], flags at [10..12]
    pkt[10] = 0x80; // broadcast flag (high byte of flags field)
    // ciaddr (client IP) at [12..16]
    pkt[12..16].copy_from_slice(&client_ip);
    // yiaddr, siaddr, giaddr = 0 (already zeroed)
    // chaddr (client MAC) at [28..34]
    pkt[28..34].copy_from_slice(&mac);
    // sname, file = 0 (already zeroed)
    // Magic cookie at offset 236
    pkt[236..240].copy_from_slice(&DHCP_MAGIC);

    // Options: message type
    pkt.push(OPT_MSG_TYPE);
    pkt.push(1);
    pkt.push(msg_type);

    // Parameter request list
    pkt.push(OPT_PARAM_LIST);
    pkt.push(3);
    pkt.push(OPT_SUBNET_MASK);
    pkt.push(OPT_ROUTER);
    pkt.push(OPT_DNS);

    // Requested IP (for DHCPREQUEST)
    if let Some(ip) = requested_ip {
        pkt.push(OPT_REQUESTED_IP);
        pkt.push(4);
        pkt.extend_from_slice(&ip);
    }

    // Server identifier (for DHCPREQUEST)
    if let Some(sid) = server_id {
        pkt.push(OPT_SERVER_ID);
        pkt.push(4);
        pkt.extend_from_slice(&sid);
    }

    pkt.push(OPT_END);

    pkt
}

/// Wrap a DHCP payload in UDP (68->67) + IPv4 (0.0.0.0->255.255.255.255) + Ethernet broadcast.
fn wrap_udp_broadcast(src_mac: [u8; 6], dhcp_payload: &[u8]) -> Vec<u8> {
    // Build UDP header
    let udp_len = (8 + dhcp_payload.len()) as u16;
    let mut udp = Vec::with_capacity(udp_len as usize);
    udp.extend_from_slice(&DHCP_CLIENT_PORT.to_be_bytes());
    udp.extend_from_slice(&DHCP_SERVER_PORT.to_be_bytes());
    udp.extend_from_slice(&udp_len.to_be_bytes());
    udp.extend_from_slice(&0u16.to_be_bytes()); // checksum 0 = disabled (valid for IPv4 UDP)
    udp.extend_from_slice(dhcp_payload);

    // Build IPv4 header
    let ip_total = (20 + udp.len()) as u16;
    let mut ip = Vec::with_capacity(ip_total as usize);
    ip.push(0x45); // version=4, ihl=5 (20 bytes)
    ip.push(0x00); // DSCP/ECN
    ip.extend_from_slice(&ip_total.to_be_bytes());
    ip.extend_from_slice(&0u16.to_be_bytes()); // identification
    ip.extend_from_slice(&0u16.to_be_bytes()); // flags + fragment offset
    ip.push(64); // TTL
    ip.push(17); // protocol = UDP
    ip.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    ip.extend_from_slice(&[0, 0, 0, 0]); // src: 0.0.0.0
    ip.extend_from_slice(&[255, 255, 255, 255]); // dst: limited broadcast

    // Compute IP header checksum over the first 20 bytes
    let cksum = internet_checksum(&ip[..20]);
    ip[10] = (cksum >> 8) as u8;
    ip[11] = (cksum & 0xFF) as u8;

    ip.extend_from_slice(&udp);

    // Wrap in Ethernet frame with broadcast destination
    ethernet::build_frame(ethernet::BROADCAST, src_mac, ethernet::EtherType::Ipv4, &ip)
}

/// Poll NIC for a specific DHCP response type. Returns parsed lease on match, None on timeout.
fn poll_for_dhcp(
    nic: &mut E1000e,
    xid: u32,
    expected_type: u8,
    timeout: Duration,
) -> Option<DhcpLease> {
    use crate::drivers::dma::dma;

    let deadline = Instant::now() + timeout;

    loop {
        if Instant::now() >= deadline {
            log!("net: dhcp: timeout waiting for msg type {}", expected_type);
            return None;
        }

        if let Some((buf, len)) = nic.receive() {
            let data = unsafe { core::slice::from_raw_parts(buf.as_ptr(), len) };
            let result = try_parse_dhcp_response(data, xid, expected_type);
            let _ = dma().dealloc(buf);
            if result.is_some() {
                return result;
            }
            // Not the packet we wanted; discard and continue polling
        }

        core::hint::spin_loop();
        // Clear any pending interrupt cause bits to keep the ring moving
        let _ = nic.handle_interrupt();
    }
}

/// Try to parse a received Ethernet frame as a DHCP response.
/// Returns Some(lease) if it matches the expected xid and message type.
fn try_parse_dhcp_response(frame: &[u8], xid: u32, expected_type: u8) -> Option<DhcpLease> {
    // Parse Ethernet header
    let (eth, eth_payload) = ethernet::parse_frame(frame)?;
    if eth.ethertype != ethernet::EtherType::Ipv4 as u16 {
        return None;
    }

    // Parse IPv4 header (minimal; skip checksum verification)
    if eth_payload.len() < 20 {
        return None;
    }
    let ip_hdr_len = ((eth_payload[0] & 0x0F) as usize) * 4;
    let ip_proto = eth_payload[9];
    if ip_proto != 17 {
        return None; // Not UDP
    }
    let ip_payload = eth_payload.get(ip_hdr_len..)?;

    // Parse UDP header
    if ip_payload.len() < 8 {
        return None;
    }
    let src_port = u16::from_be_bytes([ip_payload[0], ip_payload[1]]);
    let dst_port = u16::from_be_bytes([ip_payload[2], ip_payload[3]]);
    if src_port != DHCP_SERVER_PORT || dst_port != DHCP_CLIENT_PORT {
        return None;
    }
    let udp_payload = ip_payload.get(8..)?;

    // Parse BOOTP/DHCP fixed header (minimum 240 bytes including magic cookie)
    if udp_payload.len() < 240 {
        return None;
    }
    let pkt_xid = u32::from_be_bytes([
        udp_payload[4],
        udp_payload[5],
        udp_payload[6],
        udp_payload[7],
    ]);
    if pkt_xid != xid {
        return None;
    }

    // yiaddr = offered/assigned IP
    let offered_ip = [
        udp_payload[16],
        udp_payload[17],
        udp_payload[18],
        udp_payload[19],
    ];

    // Verify DHCP magic cookie
    if udp_payload[236..240] != DHCP_MAGIC {
        return None;
    }

    // Parse DHCP options
    let mut msg_type = 0u8;
    let mut subnet = [255u8, 255, 255, 0];
    let mut gateway = [0u8; 4];
    let mut dns = [0u8; 4];
    let mut server_id = [0u8; 4];

    let opts = &udp_payload[240..];
    let mut i = 0;
    while i < opts.len() {
        let opt = opts[i];
        if opt == OPT_END {
            break;
        }
        if opt == 0 {
            // Padding byte
            i += 1;
            continue;
        }
        if i + 1 >= opts.len() {
            break;
        }
        let opt_len = opts[i + 1] as usize;
        if i + 2 + opt_len > opts.len() {
            break;
        }
        let opt_data = &opts[i + 2..i + 2 + opt_len];

        match opt {
            OPT_MSG_TYPE if opt_len >= 1 => msg_type = opt_data[0],
            OPT_SUBNET_MASK if opt_len >= 4 => subnet.copy_from_slice(&opt_data[..4]),
            OPT_ROUTER if opt_len >= 4 => gateway.copy_from_slice(&opt_data[..4]),
            OPT_DNS if opt_len >= 4 => dns.copy_from_slice(&opt_data[..4]),
            OPT_SERVER_ID if opt_len >= 4 => server_id.copy_from_slice(&opt_data[..4]),
            _ => {}
        }

        i += 2 + opt_len;
    }

    if msg_type != expected_type {
        return None;
    }

    Some(DhcpLease {
        ip: offered_ip,
        subnet_mask: subnet,
        gateway,
        dns,
        server_id,
    })
}
