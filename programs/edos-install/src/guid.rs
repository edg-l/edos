//! GUIDs in the mixed-endian layout GPT uses.
//!
//! UEFI 2.10 §5.3.3: the first three fields of a GUID are stored
//! little-endian and the last two byte-for-byte, so the on-disk bytes are not
//! the textual order.

use std::fmt::Write as _;

/// Parse a canonical `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` GUID into its
/// on-disk byte order.
pub fn parse(text: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = text
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    if hex.len() != 16 {
        return None;
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&[hex[3], hex[2], hex[1], hex[0]]);
    out[4..6].copy_from_slice(&[hex[5], hex[4]]);
    out[6..8].copy_from_slice(&[hex[7], hex[6]]);
    out[8..16].copy_from_slice(&hex[8..16]);
    Some(out)
}

/// Render on-disk GUID bytes as canonical text.
pub fn format(guid: &[u8; 16]) -> String {
    let ordered = [
        guid[3], guid[2], guid[1], guid[0], guid[5], guid[4], guid[7], guid[6], guid[8], guid[9],
        guid[10], guid[11], guid[12], guid[13], guid[14], guid[15],
    ];
    let mut out = String::with_capacity(36);
    for (i, byte) in ordered.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A fresh random GUID, RFC 4122 version 4, already in on-disk order.
pub fn random() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    edos_lib::getrandom(&mut bytes);
    // The version and variant nibbles live in the textual fields, which map to
    // bytes 6 (little-endian field 3) and 8.
    bytes[7] = (bytes[7] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

/// EFI System Partition type GUID (UEFI 2.10 table 5-7).
pub const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
/// Microsoft basic data, what `sgdisk -t 0700` writes and what EDOS roots use.
pub const BASIC_DATA_TYPE: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
