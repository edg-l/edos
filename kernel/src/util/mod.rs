pub mod per_cpu;
pub mod trace;
pub mod uaccess;

/// Format u64 as decimal into a fixed buffer. Returns the written slice.
/// No allocation, no formatting traits -- safe for use in fault handlers.
pub fn fmt_u64(mut val: u64, buf: &mut [u8; 20]) -> &[u8] {
    if val == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = 20;
    while val > 0 {
        i -= 1;
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
    }
    &buf[i..]
}

/// Format u64 as hex into a fixed buffer. Returns the written slice.
pub fn fmt_hex(mut val: u64, buf: &mut [u8; 20]) -> &[u8] {
    if val == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = 20;
    while val > 0 {
        i -= 1;
        let nibble = (val & 0xF) as u8;
        buf[i] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        };
        val >>= 4;
    }
    &buf[i..]
}
