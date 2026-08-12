//! SSH wire types, RFC 4251 §5.
//!
//! `string` is a length-prefixed byte string and carries no NUL, `name-list` is
//! a `string` of comma-separated names, and `mpint` is a big-endian two's
//! complement integer with no leading padding beyond the sign byte. Getting
//! `mpint` wrong is invisible until the shared secret happens to have its top
//! bit set, so it has its own test.

use crate::error::{Error, Result};

/// Builds a packet payload.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// A payload that starts with a message number, which is nearly all of them.
    pub fn msg(number: u8) -> Self {
        Self { buf: vec![number] }
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    /// Bytes with no length prefix, for a field whose length is already known.
    pub fn raw(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    pub fn string(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }

    pub fn text(&mut self, v: &str) {
        self.string(v.as_bytes());
    }

    pub fn name_list(&mut self, names: &[&str]) {
        self.text(&names.join(","));
    }

    /// An unsigned integer given as big-endian bytes.
    pub fn mpint(&mut self, v: &[u8]) {
        let first = v.iter().position(|&b| b != 0).unwrap_or(v.len());
        let v = &v[first..];
        if v.is_empty() {
            self.u32(0);
        } else if v[0] & 0x80 != 0 {
            // A leading zero keeps a positive value from reading as negative.
            self.u32(v.len() as u32 + 1);
            self.buf.push(0);
            self.buf.extend_from_slice(v);
        } else {
            self.string(v);
        }
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Reads a packet payload.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(Error::Io("field overflows"))?;
        if end > self.data.len() {
            return Err(Error::Protocol("packet truncated".into()));
        }
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.byte()? != 0)
    }

    pub fn string(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    pub fn text(&mut self) -> Result<&'a str> {
        let bytes = self.string()?;
        core::str::from_utf8(bytes).map_err(|_| Error::Protocol("field is not UTF-8".into()))
    }

    /// A name-list split on commas. An empty list yields no names rather than
    /// one empty name, which is what a naive `split` would give.
    pub fn name_list(&mut self) -> Result<Vec<&'a str>> {
        let s = self.text()?;
        Ok(if s.is_empty() {
            Vec::new()
        } else {
            s.split(',').collect()
        })
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
}

/// Base64 as RFC 4648 §4, which is how a public key is written down.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpint_matches_rfc_4251() {
        let cases: &[(&[u8], &[u8])] = &[
            (&[], &[0, 0, 0, 0]),
            (&[0, 0, 0], &[0, 0, 0, 0]),
            (&[0x9a, 0x37], &[0, 0, 0, 3, 0, 0x9a, 0x37]),
            (&[0x00, 0x12, 0x34], &[0, 0, 0, 2, 0x12, 0x34]),
        ];
        for (input, want) in cases {
            let mut w = Writer::new();
            w.mpint(input);
            assert_eq!(&w.finish(), want, "mpint of {input:02x?}");
        }
    }

    #[test]
    fn reader_rejects_a_truncated_string() {
        let mut r = Reader::new(&[0, 0, 0, 8, 1, 2]);
        assert!(r.string().is_err());
    }

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
