//! Random bytes for filesystem and partition identifiers.
//!
//! Reads the kernel's random device directly rather than pulling in a RNG
//! crate: the same code has to build for the host and for EDOS userspace, and
//! `getrandom`-style crates do not know the `x86_64-unknown-edos` target.

use std::fs::File;
use std::io::{self, Read};

/// Fill `buf` from the first random device that opens.
fn fill(buf: &mut [u8]) -> io::Result<()> {
    let mut last = io::Error::other("no random device");
    for path in ["/dev/urandom", "/dev/random"] {
        match File::open(path).and_then(|mut f| f.read_exact(buf)) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// A random RFC 4122 version 4 UUID.
pub fn uuid_v4() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    fill(&mut bytes).expect("no usable random device");
    // Version 4, variant 1 (RFC 4122 §4.4).
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}
