//! Read-only qcow2 support, enough to check a filesystem inside one.
//!
//! A qcow2 image maps guest offsets to host offsets through a two-level table:
//! the L1 table (one entry per L2 table, held in memory here) and an L2 table
//! per entry (one cluster of 8-byte descriptors, read on demand). An entry of
//! zero means the cluster was never written, which reads back as zeros.
//!
//! Format reference: QEMU `docs/interop/qcow2.txt`. All header and table
//! fields are big-endian.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

pub const MAGIC: [u8; 4] = *b"QFI\xfb";

/// Host offset bits of an L1 or L2 entry; the rest are flags.
const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
/// L2 entry: the cluster is zlib-compressed rather than stored verbatim.
const L2_COMPRESSED: u64 = 1 << 62;
/// L2 entry: the cluster reads as zeros whatever its offset says.
const L2_ZERO: u64 = 1;

pub struct Qcow2 {
    file: File,
    cluster_bits: u32,
    l2_bits: u32,
    virtual_size: u64,
    l1: Vec<u64>,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

fn unsupported(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

impl Qcow2 {
    pub fn open(mut file: File) -> io::Result<Self> {
        let mut hdr = [0u8; 104];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut hdr)?;

        let version = be32(&hdr, 4);
        if version != 2 && version != 3 {
            return Err(unsupported(format!(
                "qcow2 version {version} is not v2 or v3"
            )));
        }
        if be64(&hdr, 8) != 0 {
            return Err(unsupported(
                "qcow2 image has a backing file, which fsck does not follow".into(),
            ));
        }
        let cluster_bits = be32(&hdr, 20);
        if !(9..=21).contains(&cluster_bits) {
            return Err(unsupported(format!("qcow2 cluster_bits {cluster_bits}")));
        }
        let virtual_size = be64(&hdr, 24);
        if be32(&hdr, 32) != 0 {
            return Err(unsupported("qcow2 image is encrypted".into()));
        }
        // Every incompatible feature (dirty, corrupt, external data file,
        // extended L2 entries) changes how the tables must be read, so a
        // reader that ignores one silently returns wrong data.
        if version >= 3 && be64(&hdr, 72) != 0 {
            return Err(unsupported(format!(
                "qcow2 image sets incompatible features {:#x}",
                be64(&hdr, 72)
            )));
        }

        let l1_size = be32(&hdr, 36) as usize;
        let l1_offset = be64(&hdr, 40);
        let mut l1_bytes = vec![0u8; l1_size * 8];
        if l1_size > 0 {
            file.seek(SeekFrom::Start(l1_offset))?;
            file.read_exact(&mut l1_bytes)?;
        }
        let l1 = (0..l1_size).map(|i| be64(&l1_bytes, i * 8)).collect();

        Ok(Qcow2 {
            file,
            cluster_bits,
            l2_bits: cluster_bits - 3,
            virtual_size,
            l1,
        })
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    /// Host offset of the cluster holding guest `offset`, or `None` when the
    /// cluster is unallocated and therefore reads as zeros.
    fn cluster_at(&mut self, offset: u64) -> io::Result<Option<u64>> {
        let l1_index = (offset >> (self.cluster_bits + self.l2_bits)) as usize;
        let Some(&l1_entry) = self.l1.get(l1_index) else {
            return Ok(None);
        };
        let l2_offset = l1_entry & OFFSET_MASK;
        if l2_offset == 0 {
            return Ok(None);
        }

        let l2_index = (offset >> self.cluster_bits) & ((1 << self.l2_bits) - 1);
        let mut entry = [0u8; 8];
        self.file.seek(SeekFrom::Start(l2_offset + l2_index * 8))?;
        self.file.read_exact(&mut entry)?;
        let l2_entry = u64::from_be_bytes(entry);

        if l2_entry & L2_COMPRESSED != 0 {
            return Err(unsupported(
                "qcow2 image holds compressed clusters, which fsck cannot read".into(),
            ));
        }
        let host = l2_entry & OFFSET_MASK;
        if host == 0 || l2_entry & L2_ZERO != 0 {
            return Ok(None);
        }
        Ok(Some(host))
    }

    /// Fill `buf` with guest bytes starting at `offset`, one cluster at a time.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.virtual_size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of the qcow2 image",
            ));
        }
        let cluster_size = 1u64 << self.cluster_bits;
        let mut done = 0usize;
        while done < buf.len() {
            let guest = offset + done as u64;
            let in_cluster = guest & (cluster_size - 1);
            let chunk = (cluster_size - in_cluster).min((buf.len() - done) as u64) as usize;
            let dst = &mut buf[done..done + chunk];
            match self.cluster_at(guest)? {
                Some(host) => {
                    self.file.seek(SeekFrom::Start(host + in_cluster))?;
                    self.file.read_exact(dst)?;
                }
                None => dst.fill(0),
            }
            done += chunk;
        }
        Ok(())
    }
}
