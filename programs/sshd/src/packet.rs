//! The binary packet protocol, RFC 4253 §6.
//!
//! A packet is `uint32 length || byte padding_length || payload || padding`,
//! padded so the whole thing is a multiple of the cipher block size (8 while
//! still in the clear). The MAC covers the sequence number followed by that
//! plaintext, and is appended unencrypted; the sequence number is never sent,
//! which is what binds a packet to its position in the stream.
//!
//! Both directions run `aes128-ctr`, so the keystream is continuous across
//! packets and the cipher object must outlive each one.

use aes::Aes128;
use ctr::Ctr128BE;
use ctr::cipher::{KeyIvInit, StreamCipher};
use edos_lib::net;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{Error, Result};
use crate::wire::Reader;

pub const MSG_DISCONNECT: u8 = 1;
pub const MSG_IGNORE: u8 = 2;
pub const MSG_UNIMPLEMENTED: u8 = 3;
pub const MSG_DEBUG: u8 = 4;
pub const MSG_SERVICE_REQUEST: u8 = 5;
pub const MSG_SERVICE_ACCEPT: u8 = 6;
pub const MSG_KEXINIT: u8 = 20;
pub const MSG_NEWKEYS: u8 = 21;
pub const MSG_KEX_ECDH_INIT: u8 = 30;
pub const MSG_KEX_ECDH_REPLY: u8 = 31;
pub const MSG_USERAUTH_REQUEST: u8 = 50;
pub const MSG_USERAUTH_FAILURE: u8 = 51;
pub const MSG_USERAUTH_SUCCESS: u8 = 52;
pub const MSG_GLOBAL_REQUEST: u8 = 80;
pub const MSG_REQUEST_FAILURE: u8 = 82;
pub const MSG_CHANNEL_OPEN: u8 = 90;
pub const MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
pub const MSG_CHANNEL_OPEN_FAILURE: u8 = 92;
pub const MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
pub const MSG_CHANNEL_DATA: u8 = 94;
pub const MSG_CHANNEL_EOF: u8 = 96;
pub const MSG_CHANNEL_CLOSE: u8 = 97;
pub const MSG_CHANNEL_REQUEST: u8 = 98;
pub const MSG_CHANNEL_SUCCESS: u8 = 99;
pub const MSG_CHANNEL_FAILURE: u8 = 100;

/// Disconnect reason codes, RFC 4253 §11.1.
pub const DISCONNECT_PROTOCOL_ERROR: u32 = 2;
pub const DISCONNECT_KEY_EXCHANGE_FAILED: u32 = 3;
pub const DISCONNECT_AUTH_CANCELLED: u32 = 13;
pub const DISCONNECT_BY_APPLICATION: u32 = 11;

/// Largest packet accepted. RFC 4253 §6.1 requires at least 35000 bytes of
/// payload to be supported, and nothing legitimate approaches it.
const MAX_PACKET: usize = 35000;

const AES_BLOCK: usize = 16;
const MAC_LEN: usize = 32;

type AesCtr = Ctr128BE<Aes128>;
type HmacSha256 = Hmac<Sha256>;

/// One direction's cipher and MAC, live from the `NEWKEYS` that installed them.
struct Direction {
    cipher: AesCtr,
    mac_key: [u8; 32],
}

impl Direction {
    fn new(key: &[u8; 16], iv: &[u8; 16], mac_key: &[u8; 32]) -> Self {
        Self {
            cipher: AesCtr::new(key.into(), iv.into()),
            mac_key: *mac_key,
        }
    }

    fn mac(&self, seq: u32, plaintext: &[u8]) -> [u8; MAC_LEN] {
        let mut m = <HmacSha256 as Mac>::new_from_slice(&self.mac_key)
            .expect("hmac-sha2-256 accepts any key length");
        m.update(&seq.to_be_bytes());
        m.update(plaintext);
        m.finalize().into_bytes().into()
    }
}

/// The packet layer over one connected socket.
pub struct Transport {
    fd: u64,
    tx: Option<Direction>,
    rx: Option<Direction>,
    tx_seq: u32,
    rx_seq: u32,
    /// Set by the first key exchange and never changed after, since it is what
    /// later signatures and a rekey are bound to.
    pub session_id: Option<[u8; 32]>,
}

impl Transport {
    pub fn new(fd: u64) -> Self {
        Self {
            fd,
            tx: None,
            rx: None,
            tx_seq: 0,
            rx_seq: 0,
            session_id: None,
        }
    }

    pub fn fd(&self) -> u64 {
        self.fd
    }

    pub fn set_tx_keys(&mut self, key: &[u8; 16], iv: &[u8; 16], mac: &[u8; 32]) {
        self.tx = Some(Direction::new(key, iv, mac));
    }

    pub fn set_rx_keys(&mut self, key: &[u8; 16], iv: &[u8; 16], mac: &[u8; 32]) {
        self.rx = Some(Direction::new(key, iv, mac));
    }

    fn read_exact(&self, buf: &mut [u8]) -> Result<()> {
        let mut off = 0;
        while off < buf.len() {
            match net::recv(self.fd, &mut buf[off..]) {
                Ok(0) => return Err(Error::Io("peer closed")),
                Ok(n) => off += n,
                Err(_) => return Err(Error::Io("socket read failed")),
            }
        }
        Ok(())
    }

    fn write_all(&self, buf: &[u8]) -> Result<()> {
        net::send_all(self.fd, buf).map_err(|_| Error::Io("socket write failed"))
    }

    /// Send one line of the version exchange, or any other pre-protocol text.
    pub fn write_line(&self, line: &str) -> Result<()> {
        self.write_all(line.as_bytes())?;
        self.write_all(b"\r\n")
    }

    /// Read one CRLF-terminated line, used only before the packet layer starts.
    ///
    /// RFC 4253 §4.2 lets a server send other lines before its identification,
    /// so the caller loops until one starts with `SSH-`.
    pub fn read_line(&self) -> Result<String> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            self.read_exact(&mut byte)?;
            match byte[0] {
                b'\n' => break,
                b'\r' => continue,
                b => line.push(b),
            }
            if line.len() > 255 {
                return Err(Error::Protocol("identification line too long".into()));
            }
        }
        String::from_utf8(line).map_err(|_| Error::Protocol("identification is not UTF-8".into()))
    }

    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        let block = if self.tx.is_some() { AES_BLOCK } else { 8 };

        // The length field counts towards the block multiple, and padding is at
        // least 4 bytes, so a short payload pads by a whole extra block.
        let unpadded = 5 + payload.len();
        let mut pad = block - (unpadded % block);
        if pad < 4 {
            pad += block;
        }
        while unpadded + pad < 16 {
            pad += block;
        }

        let mut frame = Vec::with_capacity(unpadded + pad + MAC_LEN);
        frame.extend_from_slice(&((1 + payload.len() + pad) as u32).to_be_bytes());
        frame.push(pad as u8);
        frame.extend_from_slice(payload);
        let pad_start = frame.len();
        frame.resize(pad_start + pad, 0);
        edos_lib::getrandom(&mut frame[pad_start..]);

        if let Some(dir) = self.tx.as_mut() {
            let mac = dir.mac(self.tx_seq, &frame);
            dir.cipher.apply_keystream(&mut frame);
            frame.extend_from_slice(&mac);
        }
        self.tx_seq = self.tx_seq.wrapping_add(1);
        self.write_all(&frame)
    }

    /// Receive one packet's payload.
    ///
    /// `IGNORE`, `DEBUG` and `UNIMPLEMENTED` are consumed here: they are legal
    /// at any point and no caller has anything to do with them. `DISCONNECT`
    /// becomes an error, since every caller would only turn it into one.
    pub fn recv(&mut self) -> Result<Vec<u8>> {
        loop {
            let payload = self.recv_raw()?;
            match payload.first() {
                Some(&MSG_IGNORE) | Some(&MSG_DEBUG) | Some(&MSG_UNIMPLEMENTED) => continue,
                Some(&MSG_DISCONNECT) => {
                    let mut r = Reader::new(&payload[1..]);
                    let code = r.u32().unwrap_or(0);
                    let text = r.text().unwrap_or("").to_string();
                    return Err(Error::PeerDisconnect(format!("code {code}: {text}")));
                }
                Some(_) => return Ok(payload),
                None => return Err(Error::Protocol("empty packet".into())),
            }
        }
    }

    fn recv_raw(&mut self) -> Result<Vec<u8>> {
        let mut plain;
        // `if let Some(dir) = &mut self.rx` cannot be used here: `read_exact`
        // below takes `&self`, and a binding into `self.rx` held across it
        // borrows `self` twice. The re-lookups are what keep the borrows short.
        #[allow(clippy::unnecessary_unwrap)]
        if self.rx.is_some() {
            // The length lives inside the encryption, so the first block has to
            // be decrypted before the rest of the packet can even be read.
            let mut head = [0u8; AES_BLOCK];
            self.read_exact(&mut head)?;
            let dir = self.rx.as_mut().expect("checked above");
            dir.cipher.apply_keystream(&mut head);

            let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
            if !(12..=MAX_PACKET).contains(&len) || !(len + 4).is_multiple_of(AES_BLOCK) {
                return Err(Error::Protocol(format!("bad packet length {len}")));
            }
            plain = head.to_vec();
            plain.resize(4 + len, 0);
            self.read_exact(&mut plain[AES_BLOCK..])?;
            let dir = self.rx.as_mut().expect("checked above");
            dir.cipher.apply_keystream(&mut plain[AES_BLOCK..]);

            let mut mac = [0u8; MAC_LEN];
            self.read_exact(&mut mac)?;
            let want = self
                .rx
                .as_ref()
                .expect("checked above")
                .mac(self.rx_seq, &plain);
            if !bool::from(subtle::ConstantTimeEq::ct_eq(&want[..], &mac[..])) {
                return Err(Error::Protocol("MAC mismatch".into()));
            }
        } else {
            let mut len_bytes = [0u8; 4];
            self.read_exact(&mut len_bytes)?;
            let len = u32::from_be_bytes(len_bytes) as usize;
            if !(12..=MAX_PACKET).contains(&len) || !(len + 4).is_multiple_of(8) {
                return Err(Error::Protocol(format!("bad packet length {len}")));
            }
            plain = vec![0u8; 4 + len];
            plain[..4].copy_from_slice(&len_bytes);
            self.read_exact(&mut plain[4..])?;
        }

        self.rx_seq = self.rx_seq.wrapping_add(1);

        let pad = plain[4] as usize;
        let payload_len = plain
            .len()
            .checked_sub(5 + pad)
            .ok_or_else(|| Error::Protocol(format!("padding {pad} exceeds packet")))?;
        Ok(plain[5..5 + payload_len].to_vec())
    }

    /// Tell the peer why the connection is ending. Best effort: the socket may
    /// already be gone, and there is nothing useful to do about that here.
    pub fn disconnect(&mut self, reason: u32, text: &str) {
        let mut w = crate::wire::Writer::msg(MSG_DISCONNECT);
        w.u32(reason);
        w.text(text);
        w.text("");
        let _ = self.send(&w.finish());
    }
}
