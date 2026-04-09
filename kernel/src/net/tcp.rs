use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::time::Duration;

use crate::thread::waitqueue::WaitQueue;
use crate::timer::Instant;

use super::checksum::internet_checksum;

// TCP flags
pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;

pub const DEFAULT_MSS: u16 = 1460; // MTU 1500 - 20 IP - 20 TCP
pub const DEFAULT_WINDOW: u16 = 16384;

#[derive(Debug, Clone)]
pub struct TcpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq_num: u32,
    pub ack_num: u32,
    pub data_offset: u8, // in 32-bit words
    pub flags: u8,
    pub window: u16,
    pub checksum: u16,
    pub urgent: u16,
}

impl TcpHeader {
    pub fn header_len(&self) -> usize {
        (self.data_offset as usize) * 4
    }
}

pub fn parse(data: &[u8], src_ip: [u8; 4], dst_ip: [u8; 4]) -> Option<(TcpHeader, &[u8])> {
    if data.len() < 20 {
        return None;
    }

    // Verify checksum with pseudo-header
    let cksum = tcp_checksum(data, src_ip, dst_ip);
    if cksum != 0 {
        return None;
    }

    let hdr = TcpHeader {
        src_port: u16::from_be_bytes([data[0], data[1]]),
        dst_port: u16::from_be_bytes([data[2], data[3]]),
        seq_num: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        ack_num: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
        data_offset: data[12] >> 4,
        flags: data[13],
        window: u16::from_be_bytes([data[14], data[15]]),
        checksum: u16::from_be_bytes([data[16], data[17]]),
        urgent: u16::from_be_bytes([data[18], data[19]]),
    };

    let hdr_len = hdr.header_len();
    if hdr_len < 20 || data.len() < hdr_len {
        return None;
    }

    Some((hdr, &data[hdr_len..]))
}

/// Parse MSS option from TCP options bytes (data between offset 20 and data_offset*4).
pub fn parse_mss(data: &[u8], data_offset: u8) -> Option<u16> {
    let opts_end = (data_offset as usize) * 4;
    if opts_end <= 20 || data.len() < opts_end {
        return None;
    }
    let opts = &data[20..opts_end];
    let mut i = 0;
    while i < opts.len() {
        match opts[i] {
            0 => break, // End of options
            1 => {
                i += 1;
            } // NOP
            2 => {
                // MSS
                if i + 3 < opts.len() && opts[i + 1] == 4 {
                    return Some(u16::from_be_bytes([opts[i + 2], opts[i + 3]]));
                }
                break;
            }
            _ => {
                if i + 1 >= opts.len() {
                    break;
                }
                let len = opts[i + 1] as usize;
                if len < 2 {
                    break;
                }
                i += len;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Build a TCP segment with checksum.
/// `options` is optional raw TCP options bytes (for MSS in SYN).
pub fn build(
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    options: &[u8],
    payload: &[u8],
) -> Vec<u8> {
    let data_offset = ((20 + options.len() + 3) / 4) as u8; // Round up to 32-bit words
    let total_hdr = data_offset as usize * 4;
    let mut pkt = Vec::with_capacity(total_hdr + payload.len());

    pkt.extend_from_slice(&src_port.to_be_bytes());
    pkt.extend_from_slice(&dst_port.to_be_bytes());
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&ack.to_be_bytes());
    pkt.push((data_offset << 4) | 0); // data offset + reserved
    pkt.push(flags);
    pkt.extend_from_slice(&window.to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    pkt.extend_from_slice(&0u16.to_be_bytes()); // urgent
    pkt.extend_from_slice(options);
    // Pad to data_offset * 4
    while pkt.len() < total_hdr {
        pkt.push(0);
    }
    pkt.extend_from_slice(payload);

    let cksum = tcp_checksum(&pkt, src_ip, dst_ip);
    pkt[16] = (cksum >> 8) as u8;
    pkt[17] = (cksum & 0xFF) as u8;
    pkt
}

/// Build MSS option bytes for SYN packets.
pub fn mss_option(mss: u16) -> [u8; 4] {
    [2, 4, (mss >> 8) as u8, (mss & 0xFF) as u8]
}

fn tcp_checksum(tcp_pkt: &[u8], src_ip: [u8; 4], dst_ip: [u8; 4]) -> u16 {
    let mut pseudo = Vec::with_capacity(12 + tcp_pkt.len());
    pseudo.extend_from_slice(&src_ip);
    pseudo.extend_from_slice(&dst_ip);
    pseudo.push(0);
    pseudo.push(6); // TCP protocol
    pseudo.extend_from_slice(&(tcp_pkt.len() as u16).to_be_bytes());
    pseudo.extend_from_slice(tcp_pkt);
    internet_checksum(&pseudo)
}

// ---------------------------------------------------------------------------
// TCP Connection State Machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

#[derive(Debug, Clone)]
pub struct RetransmitSegment {
    pub seq: u32,
    pub data: Vec<u8>, // Full TCP segment (for retransmit)
    pub sent_at: Instant,
    pub retries: u32,
}

pub struct TcpConnection {
    pub state: TcpState,
    pub local_port: u16,
    pub remote_port: u16,
    pub local_ip: [u8; 4],
    pub remote_ip: [u8; 4],

    // Send sequence space
    pub snd_una: u32, // oldest unACKed seq
    pub snd_nxt: u32, // next seq to send
    pub snd_wnd: u16, // remote window
    pub iss: u32,     // initial send seq

    // Receive sequence space
    pub rcv_nxt: u32, // next expected seq from remote
    pub rcv_wnd: u16, // our receive window
    pub irs: u32,     // initial receive seq

    // MSS negotiated
    pub mss: u16,

    // Buffers
    pub rx_buffer: VecDeque<u8>,
    pub retransmit_queue: Vec<RetransmitSegment>,

    // Waitqueues
    pub rx_wq: Arc<WaitQueue>,    // wake on data arrival or state change
    pub tx_wq: Arc<WaitQueue>,    // wake on window opening
    pub state_wq: Arc<WaitQueue>, // wake on state transitions (connect, close)

    // Close tracking
    pub fin_seq: Option<u32>, // Our FIN's sequence number (set when we send FIN)
    pub time_wait_until: Option<Instant>,
}

impl TcpConnection {
    pub fn new(local_ip: [u8; 4], local_port: u16, remote_ip: [u8; 4], remote_port: u16) -> Self {
        let iss = generate_isn();
        Self {
            state: TcpState::Closed,
            local_port,
            remote_port,
            local_ip,
            remote_ip,
            snd_una: iss,
            snd_nxt: iss,
            snd_wnd: 0,
            iss,
            rcv_nxt: 0,
            rcv_wnd: DEFAULT_WINDOW,
            irs: 0,
            mss: DEFAULT_MSS,
            rx_buffer: VecDeque::new(),
            retransmit_queue: Vec::new(),
            rx_wq: Arc::new(WaitQueue::new()),
            tx_wq: Arc::new(WaitQueue::new()),
            state_wq: Arc::new(WaitQueue::new()),
            fin_seq: None,
            time_wait_until: None,
        }
    }

    /// Build and return a TCP segment to be sent. Does NOT send it.
    /// The caller (NetStack) is responsible for wrapping in IP and sending.
    pub fn build_segment(&self, flags: u8, payload: &[u8]) -> Vec<u8> {
        let options = if flags & SYN != 0 {
            mss_option(DEFAULT_MSS).to_vec()
        } else {
            Vec::new()
        };
        build(
            self.local_port,
            self.remote_port,
            self.snd_nxt,
            self.rcv_nxt,
            flags,
            self.rcv_wnd,
            self.local_ip,
            self.remote_ip,
            &options,
            payload,
        )
    }

    /// Handle an incoming TCP segment. Returns a list of segments to send in response.
    pub fn handle_segment(
        &mut self,
        hdr: &TcpHeader,
        payload: &[u8],
        raw_data: &[u8],
    ) -> Vec<Vec<u8>> {
        let mut responses = Vec::new();

        match self.state {
            TcpState::SynSent => {
                // Expecting SYN-ACK
                if hdr.flags & RST != 0 {
                    self.state = TcpState::Closed;
                    self.state_wq.wake_all();
                    return responses;
                }
                if hdr.flags & SYN != 0 && hdr.flags & ACK != 0 {
                    // Validate ACK
                    if hdr.ack_num != self.snd_nxt {
                        return responses; // Wrong ACK, ignore
                    }
                    self.irs = hdr.seq_num;
                    self.rcv_nxt = hdr.seq_num.wrapping_add(1);
                    self.snd_una = hdr.ack_num;
                    self.snd_wnd = hdr.window;
                    // Parse MSS from SYN-ACK options
                    if let Some(mss) = parse_mss(raw_data, hdr.data_offset) {
                        self.mss = mss;
                    }
                    // Clear retransmit queue (SYN was ACKed)
                    self.retransmit_queue.clear();
                    // Send ACK
                    self.state = TcpState::Established;
                    let ack = self.build_ack();
                    responses.push(ack);
                    self.state_wq.wake_all();
                }
            }
            TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2 => {
                if hdr.flags & RST != 0 {
                    self.state = TcpState::Closed;
                    self.state_wq.wake_all();
                    self.rx_wq.wake_all();
                    self.tx_wq.wake_all();
                    return responses;
                }

                // Process ACK
                if hdr.flags & ACK != 0 {
                    self.process_ack(hdr.ack_num, hdr.window);
                }

                // Process data
                if !payload.is_empty() && hdr.seq_num == self.rcv_nxt {
                    self.rx_buffer.extend(payload);
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(payload.len() as u32);
                    self.rx_wq.wake_one();
                    // Send ACK for data
                    responses.push(self.build_ack());
                }

                // Process FIN
                if hdr.flags & FIN != 0 {
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                    responses.push(self.build_ack());

                    match self.state {
                        TcpState::Established => {
                            self.state = TcpState::CloseWait;
                            self.rx_wq.wake_all(); // Signal EOF to reader
                        }
                        TcpState::FinWait1 => {
                            if hdr.flags & ACK != 0 && self.fin_acked(hdr.ack_num) {
                                // Simultaneous close: FIN+ACK for our FIN
                                self.enter_time_wait();
                            } else {
                                self.state = TcpState::Closing;
                            }
                        }
                        TcpState::FinWait2 => {
                            self.enter_time_wait();
                        }
                        _ => {}
                    }
                    self.state_wq.wake_all();
                }

                // Handle ACK of our FIN in FinWait1
                if self.state == TcpState::FinWait1
                    && hdr.flags & ACK != 0
                    && self.fin_acked(hdr.ack_num)
                {
                    self.state = TcpState::FinWait2;
                    self.state_wq.wake_all();
                }
            }
            TcpState::CloseWait => {
                // We're waiting for the app to close. Just process ACKs.
                if hdr.flags & ACK != 0 {
                    self.process_ack(hdr.ack_num, hdr.window);
                }
            }
            TcpState::Closing => {
                if hdr.flags & ACK != 0 && self.fin_acked(hdr.ack_num) {
                    self.enter_time_wait();
                    self.state_wq.wake_all();
                }
            }
            TcpState::LastAck => {
                if hdr.flags & ACK != 0 && self.fin_acked(hdr.ack_num) {
                    self.state = TcpState::Closed;
                    self.state_wq.wake_all();
                }
            }
            _ => {}
        }

        responses
    }

    fn build_ack(&self) -> Vec<u8> {
        build(
            self.local_port,
            self.remote_port,
            self.snd_nxt,
            self.rcv_nxt,
            ACK,
            self.rcv_wnd,
            self.local_ip,
            self.remote_ip,
            &[],
            &[],
        )
    }

    fn process_ack(&mut self, ack_num: u32, window: u16) {
        // Use wrapping comparison: ack_num > snd_una means new bytes acknowledged
        if seq_gt(ack_num, self.snd_una) && seq_leq(ack_num, self.snd_nxt) {
            self.snd_una = ack_num;
            self.snd_wnd = window;
            // Remove acknowledged segments from retransmit queue
            self.retransmit_queue.retain(|seg| {
                let seg_end = seg.seq.wrapping_add(seg.data.len() as u32);
                seq_gt(seg_end, ack_num)
            });
            self.tx_wq.wake_all();
        }
    }

    fn fin_acked(&self, ack_num: u32) -> bool {
        if let Some(fin_seq) = self.fin_seq {
            seq_gt(ack_num, fin_seq)
        } else {
            false
        }
    }

    fn enter_time_wait(&mut self) {
        self.state = TcpState::TimeWait;
        self.time_wait_until = Some(Instant::now() + Duration::from_secs(5));
    }

    /// Build a SYN segment for active open. Caller sends it.
    pub fn build_syn(&mut self) -> Vec<u8> {
        self.state = TcpState::SynSent;
        let options = mss_option(DEFAULT_MSS);
        let seg = build(
            self.local_port,
            self.remote_port,
            self.iss,
            0,
            SYN,
            self.rcv_wnd,
            self.local_ip,
            self.remote_ip,
            &options,
            &[],
        );
        self.snd_nxt = self.iss.wrapping_add(1); // SYN consumes one seq
        // Add to retransmit queue
        self.retransmit_queue.push(RetransmitSegment {
            seq: self.iss,
            data: seg.clone(),
            sent_at: Instant::now(),
            retries: 0,
        });
        seg
    }

    /// Build a FIN segment for active close. Caller sends it.
    pub fn build_fin(&mut self) -> Option<Vec<u8>> {
        match self.state {
            TcpState::Established => {
                self.state = TcpState::FinWait1;
            }
            TcpState::CloseWait => {
                self.state = TcpState::LastAck;
            }
            _ => return None,
        }
        let fin_seq = self.snd_nxt;
        self.fin_seq = Some(fin_seq);
        let seg = build(
            self.local_port,
            self.remote_port,
            fin_seq,
            self.rcv_nxt,
            FIN | ACK,
            self.rcv_wnd,
            self.local_ip,
            self.remote_ip,
            &[],
            &[],
        );
        self.snd_nxt = self.snd_nxt.wrapping_add(1); // FIN consumes one seq
        self.retransmit_queue.push(RetransmitSegment {
            seq: fin_seq,
            data: seg.clone(),
            sent_at: Instant::now(),
            retries: 0,
        });
        Some(seg)
    }

    /// Build data segments for transmission. Respects MSS and send window.
    /// Returns a list of segments to send.
    pub fn build_data_segments(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut segments = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            let available_window = self.snd_wnd as u32;
            let in_flight = self.snd_nxt.wrapping_sub(self.snd_una);
            if in_flight >= available_window {
                break; // Window full
            }
            let window_left = available_window.saturating_sub(in_flight) as usize;
            let chunk_size = data[offset..].len().min(self.mss as usize).min(window_left);
            if chunk_size == 0 {
                break;
            }

            let chunk = &data[offset..offset + chunk_size];
            let seg = build(
                self.local_port,
                self.remote_port,
                self.snd_nxt,
                self.rcv_nxt,
                ACK | PSH,
                self.rcv_wnd,
                self.local_ip,
                self.remote_ip,
                &[],
                chunk,
            );

            self.retransmit_queue.push(RetransmitSegment {
                seq: self.snd_nxt,
                data: seg.clone(),
                sent_at: Instant::now(),
                retries: 0,
            });

            self.snd_nxt = self.snd_nxt.wrapping_add(chunk_size as u32);
            offset += chunk_size;
            segments.push(seg);
        }

        segments
    }

    /// Check retransmit queue for timed-out segments. Returns segments to resend.
    pub fn check_retransmit(&mut self) -> Vec<Vec<u8>> {
        let now = Instant::now();
        let mut resends = Vec::new();
        let mut dead = false;

        for seg in &mut self.retransmit_queue {
            let rto = Duration::from_secs(1) * (1 << seg.retries.min(5));
            if now.duration_since(seg.sent_at) >= rto {
                if seg.retries >= 5 {
                    dead = true;
                    break;
                }
                seg.retries += 1;
                seg.sent_at = now;
                resends.push(seg.data.clone());
            }
        }

        if dead {
            // Connection dead - send RST and close
            let rst = build(
                self.local_port,
                self.remote_port,
                self.snd_nxt,
                0,
                RST,
                0,
                self.local_ip,
                self.remote_ip,
                &[],
                &[],
            );
            resends.push(rst);
            self.state = TcpState::Closed;
            self.retransmit_queue.clear();
            self.state_wq.wake_all();
            self.rx_wq.wake_all();
            self.tx_wq.wake_all();
        }

        resends
    }
}

// ---------------------------------------------------------------------------
// Sequence number helpers
// ---------------------------------------------------------------------------

/// Wrapping sequence number comparison: a > b
fn seq_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Wrapping sequence number comparison: a <= b
fn seq_leq(a: u32, b: u32) -> bool {
    !seq_gt(a, b)
}

/// Generate a random initial sequence number.
fn generate_isn() -> u32 {
    let mut buf = [0u8; 4];
    crate::drivers::random::fill_bytes(&mut buf);
    u32::from_ne_bytes(buf)
}
