//! The POSIX ustar header block.
//!
//! Shared by `tar`, which reads and writes archives, and `grab`, which unpacks
//! packages. One decoder, so a package that `tar` can read is a package `grab`
//! can install and the two can never disagree about what an archive says.

//! The ustar header block (POSIX.1-1988), 512 bytes, all fields ASCII.

pub const BLOCK: usize = 512;

/// Field offsets and widths, in the order they appear on disk.
const NAME: (usize, usize) = (0, 100);
const MODE: (usize, usize) = (100, 8);
const UID: (usize, usize) = (108, 8);
const GID: (usize, usize) = (116, 8);
const SIZE: (usize, usize) = (124, 12);
const MTIME: (usize, usize) = (136, 12);
const CHKSUM: (usize, usize) = (148, 8);
const TYPEFLAG: usize = 156;
const LINKNAME: (usize, usize) = (157, 100);
const MAGIC: (usize, usize) = (257, 6);
const VERSION: (usize, usize) = (263, 2);
const UNAME: (usize, usize) = (265, 32);
const GNAME: (usize, usize) = (297, 32);
const PREFIX: (usize, usize) = (345, 155);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

impl Kind {
    fn flag(self) -> u8 {
        match self {
            Kind::File => b'0',
            Kind::Dir => b'5',
            Kind::Symlink => b'2',
        }
    }

    fn from_flag(flag: u8) -> Option<Kind> {
        match flag {
            b'0' | 0 => Some(Kind::File),
            b'5' => Some(Kind::Dir),
            b'2' => Some(Kind::Symlink),
            _ => None,
        }
    }
}

/// One archive member, independent of how it is stored on disk.
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: u64,
    pub mode: u32,
    pub link: String,
}

/// Write `value` as zero-padded octal filling `width - 1` digits, NUL
/// terminated. A value too large for the field is truncated to its low bits,
/// which cannot happen for the sizes this filesystem can hold.
fn put_octal(block: &mut [u8], (off, width): (usize, usize), value: u64) {
    let digits = width - 1;
    let mut v = value;
    for i in (0..digits).rev() {
        block[off + i] = b'0' + (v & 7) as u8;
        v >>= 3;
    }
    block[off + digits] = 0;
}

fn put_str(block: &mut [u8], (off, width): (usize, usize), s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(width);
    block[off..off + n].copy_from_slice(&bytes[..n]);
}

fn get_str(block: &[u8], (off, width): (usize, usize)) -> String {
    let field = &block[off..off + width];
    let end = field.iter().position(|&b| b == 0).unwrap_or(width);
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Parse an octal field, stopping at the first byte that is not a digit.
/// Leading spaces and a trailing NUL or space are both accepted, since
/// implementations disagree on the terminator.
fn get_octal(block: &[u8], (off, width): (usize, usize)) -> u64 {
    let mut value: u64 = 0;
    for &b in &block[off..off + width] {
        match b {
            b'0'..=b'7' => value = value * 8 + (b - b'0') as u64,
            b' ' if value == 0 => continue,
            _ => break,
        }
    }
    value
}

/// The header checksum is the unsigned sum of every byte with the checksum
/// field itself read as eight spaces.
fn checksum(block: &[u8]) -> u32 {
    let (off, width) = CHKSUM;
    block
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if i >= off && i < off + width {
                b' ' as u32
            } else {
                b as u32
            }
        })
        .sum()
}

/// Split a path into the `prefix`/`name` pair ustar stores it as. Returns
/// `None` when no split leaves both halves within their fields.
fn split_name(name: &str) -> Option<(&str, &str)> {
    if name.len() <= NAME.1 {
        return Some(("", name));
    }
    // The longest prefix that fits, so the remainder is as short as possible.
    let mut best: Option<(&str, &str)> = None;
    for (i, _) in name.match_indices('/') {
        let (prefix, rest) = (&name[..i], &name[i + 1..]);
        if prefix.len() <= PREFIX.1 && !rest.is_empty() && rest.len() <= NAME.1 {
            best = Some((prefix, rest));
        }
    }
    best
}

/// Serialize an entry into its header block, or `None` if its name cannot be
/// represented in ustar.
pub fn encode(entry: &Entry) -> Option<[u8; BLOCK]> {
    let mut block = [0u8; BLOCK];
    let (prefix, name) = split_name(&entry.name)?;

    put_str(&mut block, NAME, name);
    put_str(&mut block, PREFIX, prefix);
    put_octal(&mut block, MODE, entry.mode as u64);
    put_octal(&mut block, UID, 0);
    put_octal(&mut block, GID, 0);
    put_octal(
        &mut block,
        SIZE,
        if entry.kind == Kind::File {
            entry.size
        } else {
            0
        },
    );
    put_octal(&mut block, MTIME, entry.mtime);
    block[TYPEFLAG] = entry.kind.flag();
    put_str(&mut block, LINKNAME, &entry.link);
    put_str(&mut block, MAGIC, "ustar");
    put_str(&mut block, VERSION, "00");
    put_str(&mut block, UNAME, "root");
    put_str(&mut block, GNAME, "root");

    // Six octal digits, a NUL, then a space: the form every reader accepts.
    let sum = checksum(&block);
    put_octal(&mut block, (CHKSUM.0, 7), sum as u64);
    block[CHKSUM.0 + 7] = b' ';
    Some(block)
}

pub enum Decoded {
    Entry(Entry),
    /// An all-zero block: the archive's end marker.
    End,
}

/// Parse a header block. `Err` carries a reason the block is not a header.
pub fn decode(block: &[u8; BLOCK]) -> Result<Decoded, String> {
    if block.iter().all(|&b| b == 0) {
        return Ok(Decoded::End);
    }

    let stored = get_octal(block, CHKSUM);
    if stored != checksum(block) as u64 {
        return Err("bad header checksum".to_string());
    }

    let kind = Kind::from_flag(block[TYPEFLAG])
        .ok_or_else(|| format!("unsupported entry type '{}'", block[TYPEFLAG] as char))?;

    let prefix = get_str(block, PREFIX);
    let name = get_str(block, NAME);
    let name = if prefix.is_empty() {
        name
    } else {
        format!("{}/{}", prefix, name)
    };

    Ok(Decoded::Entry(Entry {
        name,
        kind,
        size: if kind == Kind::File {
            get_octal(block, SIZE)
        } else {
            0
        },
        mtime: get_octal(block, MTIME),
        mode: get_octal(block, MODE) as u32,
        link: get_str(block, LINKNAME),
    }))
}
