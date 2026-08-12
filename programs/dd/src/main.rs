//! Copy a file block by block, with an explicit block size and offsets.
//!
//! The point is reaching the raw block devices under `/dev` by hand: every
//! other program in the system goes through a filesystem, so a sector read or
//! a partition-table write has no interactive path without this.

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process;
use std::time::Instant;

const DEFAULT_BS: u64 = 512;

fn usage() -> ! {
    eprintln!(
        "Usage: dd [OPERAND]...

Operands:
  if=FILE     read from FILE instead of stdin
  of=FILE     write to FILE instead of stdout
  bs=BYTES    read and write BYTES per block (default 512)
  count=N     copy only N input blocks
  skip=N      skip N input blocks before copying
  seek=N      skip N output blocks before writing
  conv=LIST   comma-separated: notrunc, sync
  status=none suppress the transfer summary

BYTES may carry a suffix: c=1, b=512, k=1024, M, G."
    );
    process::exit(1);
}

fn fail(msg: &str) -> ! {
    eprintln!("dd: {msg}");
    process::exit(1);
}

fn parse_size(operand: &str, s: &str) -> u64 {
    let (num, mult) = match s.chars().last() {
        Some('c') => (&s[..s.len() - 1], 1),
        Some('b') => (&s[..s.len() - 1], 512),
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    match num.parse::<u64>() {
        Ok(v) => v * mult,
        Err(_) => fail(&format!("invalid number for {operand}: {s}")),
    }
}

/// Read up to `buf.len()` bytes, stopping only at EOF.
///
/// A short read is a property of the source, not of the data, so a block
/// arriving in pieces still counts as one whole block.
fn read_block(src: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match src.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Advance a non-seekable source by reading and discarding.
fn discard(src: &mut dyn Read, mut bytes: u64) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    while bytes > 0 {
        let want = bytes.min(buf.len() as u64) as usize;
        let n = read_block(src, &mut buf[..want])?;
        if n == 0 {
            break;
        }
        bytes -= n as u64;
    }
    Ok(())
}

fn rate(bytes: u64, secs: f64) -> String {
    if secs <= 0.0 {
        return "inf".to_string();
    }
    let per_sec = bytes as f64 / secs;
    if per_sec >= 1024.0 * 1024.0 {
        format!("{:.1} MiB/s", per_sec / (1024.0 * 1024.0))
    } else if per_sec >= 1024.0 {
        format!("{:.1} KiB/s", per_sec / 1024.0)
    } else {
        format!("{per_sec:.0} B/s")
    }
}

fn main() {
    let mut infile: Option<String> = None;
    let mut outfile: Option<String> = None;
    let mut bs = DEFAULT_BS;
    let mut count: Option<u64> = None;
    let mut skip = 0u64;
    let mut seek = 0u64;
    let mut notrunc = false;
    let mut sync_pad = false;
    let mut status_none = false;

    for arg in env::args().skip(1) {
        if arg == "--help" {
            usage();
        }
        let Some((key, value)) = arg.split_once('=') else {
            fail(&format!("unrecognized operand: {arg}"));
        };
        match key {
            "if" => infile = Some(value.to_string()),
            "of" => outfile = Some(value.to_string()),
            "bs" => bs = parse_size("bs", value),
            "count" => count = Some(parse_size("count", value)),
            "skip" => skip = parse_size("skip", value),
            "seek" => seek = parse_size("seek", value),
            "conv" => {
                for c in value.split(',') {
                    match c {
                        "notrunc" => notrunc = true,
                        "sync" => sync_pad = true,
                        _ => fail(&format!("unknown conversion: {c}")),
                    }
                }
            }
            "status" => match value {
                "none" => status_none = true,
                "progress" => status_none = false,
                _ => fail(&format!("unknown status level: {value}")),
            },
            _ => fail(&format!("unrecognized operand: {arg}")),
        }
    }

    if bs == 0 {
        fail("bs must be greater than zero");
    }
    let bs_usize = bs as usize;

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut src: Box<dyn Read> = match &infile {
        Some(path) => {
            let mut f = File::open(path).unwrap_or_else(|e| fail(&format!("{path}: {e}")));
            if skip > 0 {
                f.seek(SeekFrom::Start(skip * bs))
                    .unwrap_or_else(|e| fail(&format!("{path}: seek: {e}")));
            }
            Box::new(f)
        }
        None => {
            let mut s = stdin.lock();
            if skip > 0 {
                discard(&mut s, skip * bs).unwrap_or_else(|e| fail(&format!("stdin: {e}")));
            }
            Box::new(s)
        }
    };

    let mut dst: Box<dyn Write> = match &outfile {
        Some(path) => {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .unwrap_or_else(|e| fail(&format!("{path}: {e}")));
            // `seek=` places the copy inside a file that keeps what surrounds
            // it, so truncation is what `notrunc` turns off and what an offset
            // implies. A block device has nothing to truncate and ignores it.
            if !notrunc && seek == 0 {
                let _ = f.set_len(0);
            }
            if seek > 0 {
                f.seek(SeekFrom::Start(seek * bs))
                    .unwrap_or_else(|e| fail(&format!("{path}: seek: {e}")));
            }
            Box::new(f)
        }
        None => Box::new(stdout.lock()),
    };

    let mut buf = vec![0u8; bs_usize];
    let (mut full_in, mut part_in) = (0u64, 0u64);
    let (mut full_out, mut part_out) = (0u64, 0u64);
    let mut copied = 0u64;
    let start = Instant::now();

    loop {
        if count.is_some_and(|c| full_in + part_in >= c) {
            break;
        }
        let n = read_block(&mut *src, &mut buf).unwrap_or_else(|e| fail(&format!("read: {e}")));
        if n == 0 {
            break;
        }
        if n == bs_usize {
            full_in += 1;
        } else {
            part_in += 1;
        }

        let out = if n < bs_usize && sync_pad {
            buf[n..].fill(0);
            bs_usize
        } else {
            n
        };
        dst.write_all(&buf[..out])
            .unwrap_or_else(|e| fail(&format!("write: {e}")));
        if out == bs_usize {
            full_out += 1;
        } else {
            part_out += 1;
        }
        copied += out as u64;
    }

    dst.flush().unwrap_or_else(|e| fail(&format!("write: {e}")));
    let elapsed = start.elapsed().as_secs_f64();

    if !status_none {
        eprintln!("{full_in}+{part_in} records in");
        eprintln!("{full_out}+{part_out} records out");
        eprintln!(
            "{copied} bytes copied, {elapsed:.3} s, {}",
            rate(copied, elapsed)
        );
    }
}
