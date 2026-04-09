use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::timer::Instant;

use super::ipv4::Ipv4Header;

/// Maximum number of concurrent reassembly entries.
const MAX_ENTRIES: usize = 32;
/// Reassembly timeout: drop incomplete entries after this duration.
const TIMEOUT_SECS: u64 = 30;
/// Maximum reassembled datagram size (IP max = 65535).
const MAX_DATAGRAM: usize = 65535;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FragKey {
    src: [u8; 4],
    dst: [u8; 4],
    protocol: u8,
    identification: u16,
}

struct ReassemblyEntry {
    /// Reassembled payload buffer.
    buffer: Vec<u8>,
    /// Sorted, non-overlapping received byte ranges: [(start, end), ...].
    ranges: Vec<(usize, usize)>,
    /// Total payload length, known once the last fragment (MF=0) arrives.
    total_len: Option<usize>,
    /// Protocol and addresses for rebuilding the header.
    header: Ipv4Header,
    created: Instant,
}

impl ReassemblyEntry {
    fn new(header: Ipv4Header) -> Self {
        Self {
            buffer: Vec::new(),
            ranges: Vec::new(),
            total_len: None,
            header,
            created: Instant::now(),
        }
    }

    /// Insert a fragment. Returns true if the datagram is now complete.
    fn insert(&mut self, offset: usize, mf: bool, data: &[u8]) -> bool {
        let end = offset + data.len();
        if end > MAX_DATAGRAM {
            return false;
        }

        // Grow buffer if needed.
        if end > self.buffer.len() {
            self.buffer.resize(end, 0);
        }
        self.buffer[offset..end].copy_from_slice(data);

        // Record the last fragment's end as total length.
        if !mf {
            self.total_len = Some(end);
        }

        // Merge this range into the sorted interval list.
        self.add_range(offset, end);

        // Complete when we know the total and have one contiguous range [0, total).
        if let Some(total) = self.total_len {
            self.ranges.len() == 1 && self.ranges[0] == (0, total)
        } else {
            false
        }
    }

    fn add_range(&mut self, start: usize, end: usize) {
        let mut new_start = start;
        let mut new_end = end;

        // Merge with any overlapping or adjacent existing ranges.
        self.ranges.retain(|&(s, e)| {
            if s <= new_end && new_start <= e {
                // Overlapping or adjacent: absorb into new range.
                new_start = new_start.min(s);
                new_end = new_end.max(e);
                false
            } else {
                true
            }
        });

        // Insert merged range in sorted position.
        let pos = self.ranges.partition_point(|&(s, _)| s < new_start);
        self.ranges.insert(pos, (new_start, new_end));
    }

    fn is_expired(&self) -> bool {
        self.created.elapsed().as_secs() >= TIMEOUT_SECS
    }
}

pub struct ReassemblyTable {
    entries: BTreeMap<FragKey, ReassemblyEntry>,
}

impl ReassemblyTable {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Process a potentially fragmented IP packet.
    ///
    /// - Non-fragments are returned as-is: `Some((header, payload))`.
    /// - Fragments are buffered; returns `Some` only when complete.
    /// - Returns `None` if the fragment was buffered (incomplete) or invalid.
    pub fn process(&mut self, hdr: &Ipv4Header, payload: &[u8]) -> Option<(Ipv4Header, Vec<u8>)> {
        let frag_offset = (hdr.flags_fragment & 0x1FFF) as usize * 8;
        let mf = hdr.flags_fragment & 0x2000 != 0;

        // Not a fragment: return directly.
        if frag_offset == 0 && !mf {
            return Some((hdr.clone(), payload.to_vec()));
        }

        let key = FragKey {
            src: hdr.src_addr,
            dst: hdr.dst_addr,
            protocol: hdr.protocol,
            identification: hdr.identification,
        };

        // Evict expired entries first.
        self.entries.retain(|_, e| !e.is_expired());

        // Enforce entry limit to prevent memory exhaustion.
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_ENTRIES {
            return None; // Drop: too many concurrent reassemblies.
        }

        let entry = self
            .entries
            .entry(key)
            .or_insert_with(|| ReassemblyEntry::new(hdr.clone()));

        if entry.insert(frag_offset, mf, payload) {
            let entry = self.entries.remove(&key).unwrap();
            let total = entry.total_len.unwrap();
            let mut data = entry.buffer;
            data.truncate(total);
            Some((entry.header, data))
        } else {
            None
        }
    }
}
