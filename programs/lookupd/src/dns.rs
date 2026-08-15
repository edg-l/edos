//! DNS message handling and the answer cache.
//!
//! Deliberately depends on nothing but `std`, so `scripts/host-tests` can run
//! its tests in a second rather than through a guest boot. Time is passed in
//! as monotonic seconds for the same reason.
//!
//! `lookupd` is a forwarding cache, not a resolver: it reads the question out
//! of a query, and stores the upstream's response verbatim. Serving a hit means
//! rewriting the transaction id and decrementing the TTLs. Nothing here parses
//! an address or understands a record type beyond SOA, which RFC 2308 needs for
//! negative caching.

use std::collections::HashMap;

/// Bytes in a DNS header (RFC 1035 section 4.1.1).
pub const HEADER_LEN: usize = 12;

/// The largest answer kept. A response that does not fit in a datagram sets TC
/// and is passed through uncached, so nothing larger ever reaches the cache.
pub const MAX_MESSAGE: usize = 512;

/// How many answers are kept. The one dial: everything else here is derived
/// from the protocol.
pub const MAX_ENTRIES: usize = 512;

/// A TTL is honoured as given but not past this. A zone that asks for a week
/// does not get to pin an entry for a week.
pub const TTL_CEILING: u32 = 86_400;

/// The ceiling on a negative answer, which RFC 2308 section 5 asks for
/// explicitly. Typos are most of what anything resolves twice, and they should
/// stop being wrong quickly once the name appears.
pub const NEGATIVE_TTL_CEILING: u32 = 300;

/// Type numbers this file needs to recognise (RFC 1035 section 3.2.2).
const TYPE_SOA: u16 = 6;
/// OPT (RFC 6891) stores flags in the TTL field, so it is never adjusted.
const TYPE_OPT: u16 = 41;

/// The question a query asks, and the cache key.
///
/// `name` is the wire-form label sequence, lowercased: DNS names are compared
/// case-insensitively (RFC 4343), so `EXAMPLE.com` and `example.com` must not
/// be two entries.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Question {
    pub name: Vec<u8>,
    pub qtype: u16,
    pub qclass: u16,
}

fn u16_at(pkt: &[u8], pos: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*pkt.get(pos)?, *pkt.get(pos + 1)?]))
}

fn u32_at(pkt: &[u8], pos: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *pkt.get(pos)?,
        *pkt.get(pos + 1)?,
        *pkt.get(pos + 2)?,
        *pkt.get(pos + 3)?,
    ]))
}

/// The transaction id a client used, which its answer has to carry back.
pub fn id(pkt: &[u8]) -> Option<u16> {
    u16_at(pkt, 0)
}

/// Stamp a cached answer with the id of the query being answered.
pub fn set_id(pkt: &mut [u8], id: u16) {
    if pkt.len() >= 2 {
        pkt[..2].copy_from_slice(&id.to_be_bytes());
    }
}

/// TC: the answer did not fit in a datagram (RFC 1035 section 4.1.1).
pub fn truncated(pkt: &[u8]) -> bool {
    pkt.get(2).is_some_and(|f| f & 0x02 != 0)
}

/// RCODE: 0 NOERROR, 3 NXDOMAIN.
pub fn rcode(pkt: &[u8]) -> u8 {
    pkt.get(3).map_or(0, |f| f & 0x0f)
}

fn counts(pkt: &[u8]) -> Option<(u16, u16, u16, u16)> {
    Some((
        u16_at(pkt, 4)?,
        u16_at(pkt, 6)?,
        u16_at(pkt, 8)?,
        u16_at(pkt, 10)?,
    ))
}

/// Step over a name, whether written out or compressed.
///
/// A compression pointer (RFC 1035 section 4.1.4, top two bits set) is two
/// bytes and ends the name, so this never follows one: nothing here needs the
/// name's value, only where it stops.
fn skip_name(pkt: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *pkt.get(pos)? as usize;
        if len & 0xc0 == 0xc0 {
            return Some(pos + 2).filter(|end| *end <= pkt.len());
        }
        if len & 0xc0 != 0 {
            return None;
        }
        pos += 1;
        if len == 0 {
            return Some(pos);
        }
        pos += len;
        if pos > pkt.len() {
            return None;
        }
    }
}

/// Read the single question out of a query, and say where it ends.
///
/// A message with anything other than one question is not something this cache
/// can key, and is refused rather than guessed at; the caller forwards it
/// verbatim instead.
pub fn parse_question(pkt: &[u8]) -> Option<(Question, usize)> {
    let (qdcount, _, _, _) = counts(pkt)?;
    if qdcount != 1 {
        return None;
    }

    let start = HEADER_LEN;
    let mut pos = start;
    loop {
        let len = *pkt.get(pos)? as usize;
        // A question name is never compressed: there is nothing before it to
        // point at.
        if len & 0xc0 != 0 {
            return None;
        }
        pos += 1;
        if len == 0 {
            break;
        }
        pos += len;
        if pos > pkt.len() {
            return None;
        }
    }

    let mut name = pkt.get(start..pos)?.to_vec();
    name.make_ascii_lowercase();
    let qtype = u16_at(pkt, pos)?;
    let qclass = u16_at(pkt, pos + 2)?;

    Some((
        Question {
            name,
            qtype,
            qclass,
        },
        pos + 4,
    ))
}

/// Where each resource record's TTL field starts, in order.
///
/// Answers, authority and additional records all carry one. OPT is skipped: its
/// TTL field holds extended flags, and decrementing those corrupts the record.
fn ttl_offsets(pkt: &[u8]) -> Option<Vec<(usize, u16)>> {
    let (_, an, ns, ar) = counts(pkt)?;
    let (_, mut pos) = parse_question(pkt)?;

    let mut out = Vec::new();
    for _ in 0..(an as u32 + ns as u32 + ar as u32) {
        pos = skip_name(pkt, pos)?;
        let rtype = u16_at(pkt, pos)?;
        let ttl_at = pos + 4;
        let rdlen = u16_at(pkt, pos + 8)? as usize;
        if rtype != TYPE_OPT {
            out.push((ttl_at, rtype));
        }
        pos = pos + 10 + rdlen;
        if pos > pkt.len() {
            return None;
        }
    }
    Some(out)
}

/// How long the whole message may be cached: the smallest TTL in it, clamped.
///
/// A message is cached as a unit, so it expires when its shortest-lived record
/// does. Returns None when there is nothing to cache.
pub fn positive_ttl(pkt: &[u8]) -> Option<u32> {
    let offsets = ttl_offsets(pkt)?;
    offsets
        .iter()
        .filter_map(|(at, _)| u32_at(pkt, *at))
        .min()
        .map(|ttl| ttl.min(TTL_CEILING))
}

/// How long "this name does not exist" may be believed, per RFC 2308.
///
/// The answer is in the SOA the server put in the authority section: the
/// smaller of that record's own TTL and its MINIMUM field, which is the last
/// four bytes of its RDATA.
pub fn negative_ttl(pkt: &[u8]) -> Option<u32> {
    let (_, an, _, _) = counts(pkt)?;
    if rcode(pkt) != 3 && an != 0 {
        return None;
    }

    let (_, mut pos) = parse_question(pkt)?;
    let (_, an, ns, _) = counts(pkt)?;
    for i in 0..(an as u32 + ns as u32) {
        pos = skip_name(pkt, pos)?;
        let rtype = u16_at(pkt, pos)?;
        let ttl = u32_at(pkt, pos + 4)?;
        let rdlen = u16_at(pkt, pos + 8)? as usize;
        let rdata = pos + 10;
        if rtype == TYPE_SOA && i >= an as u32 {
            let minimum = u32_at(pkt, rdata + rdlen.checked_sub(4)?)?;
            return Some(ttl.min(minimum).min(NEGATIVE_TTL_CEILING));
        }
        pos = rdata + rdlen;
        if pos > pkt.len() {
            return None;
        }
    }
    None
}

/// Age a stored answer by `secs` before handing it out, so a client sees the
/// time remaining rather than the time the upstream granted.
pub fn decrement_ttls(pkt: &mut [u8], secs: u32) {
    let Some(offsets) = ttl_offsets(pkt) else {
        return;
    };
    for (at, _) in offsets {
        let Some(ttl) = u32_at(pkt, at) else { continue };
        pkt[at..at + 4].copy_from_slice(&ttl.saturating_sub(secs).to_be_bytes());
    }
}

struct Entry {
    bytes: Vec<u8>,
    stored_at: u64,
    ttl: u32,
    /// Bumped on every hit, so eviction can drop the least recently used.
    used_at: u64,
}

/// What the cache had for a question.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    /// A live answer, already aged and ready to send.
    Fresh(Vec<u8>),
    /// An expired answer, worth sending only if the upstream cannot be reached
    /// (RFC 8767).
    Stale(Vec<u8>),
    Miss,
}

/// Answers keyed by question, bounded, evicted least-recently-used first.
pub struct Cache {
    entries: HashMap<Question, Entry>,
    capacity: usize,
    clock: u64,
    pub hits: u64,
    pub misses: u64,
    pub stale_served: u64,
}

impl Cache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity: capacity.max(1),
            clock: 0,
            hits: 0,
            misses: 0,
            stale_served: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Look `q` up as of `now`, in monotonic seconds.
    ///
    /// A hit is returned already aged: the caller only has to stamp the
    /// client's transaction id on it.
    pub fn get(&mut self, q: &Question, now: u64) -> Lookup {
        self.clock += 1;
        let clock = self.clock;
        let Some(entry) = self.entries.get_mut(q) else {
            self.misses += 1;
            return Lookup::Miss;
        };

        let age = now.saturating_sub(entry.stored_at);
        let mut bytes = entry.bytes.clone();
        if age < entry.ttl as u64 {
            entry.used_at = clock;
            self.hits += 1;
            decrement_ttls(&mut bytes, age as u32);
            Lookup::Fresh(bytes)
        } else {
            // A stale answer keeps its remaining TTL at zero rather than
            // wrapping: a client must not be told a dead record is live.
            decrement_ttls(&mut bytes, u32::MAX);
            Lookup::Stale(bytes)
        }
    }

    /// Record `bytes` as the answer to `q`, live for `ttl` seconds from `now`.
    ///
    /// A zero TTL is honoured by storing nothing: the upstream asked for the
    /// answer not to be reused.
    pub fn insert(&mut self, q: Question, bytes: Vec<u8>, now: u64, ttl: u32) {
        if ttl == 0 || bytes.len() > MAX_MESSAGE {
            return;
        }
        self.clock += 1;
        if self.entries.len() >= self.capacity && !self.entries.contains_key(&q) {
            self.evict_one();
        }
        self.entries.insert(
            q,
            Entry {
                bytes,
                stored_at: now,
                ttl,
                used_at: self.clock,
            },
        );
    }

    /// Count a stale answer that was sent because the upstream did not reply.
    pub fn count_stale_served(&mut self) {
        self.stale_served += 1;
    }

    fn evict_one(&mut self) {
        let Some(victim) = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.used_at)
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        self.entries.remove(&victim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a query for `name`, qtype A, qclass IN.
    fn query(id: u16, name: &str) -> Vec<u8> {
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&id.to_be_bytes());
        pkt.extend_from_slice(&[0x01, 0x00]);
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&[0; 6]);
        for label in name.split('.') {
            pkt.push(label.len() as u8);
            pkt.extend_from_slice(label.as_bytes());
        }
        pkt.push(0);
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt
    }

    /// A response to `query(name)` carrying one A record per TTL given, each
    /// naming the question by a compression pointer the way a real server does.
    fn response(id: u16, name: &str, ttls: &[u32]) -> Vec<u8> {
        let mut pkt = query(id, name);
        pkt[2] = 0x81;
        pkt[3] = 0x80;
        pkt[6..8].copy_from_slice(&(ttls.len() as u16).to_be_bytes());
        for (i, ttl) in ttls.iter().enumerate() {
            pkt.extend_from_slice(&[0xc0, HEADER_LEN as u8]);
            pkt.extend_from_slice(&1u16.to_be_bytes());
            pkt.extend_from_slice(&1u16.to_be_bytes());
            pkt.extend_from_slice(&ttl.to_be_bytes());
            pkt.extend_from_slice(&4u16.to_be_bytes());
            pkt.extend_from_slice(&[10, 0, 0, i as u8]);
        }
        pkt
    }

    /// NXDOMAIN with an SOA in the authority section, MINIMUM as given.
    fn nxdomain(id: u16, name: &str, soa_ttl: u32, minimum: u32) -> Vec<u8> {
        let mut pkt = query(id, name);
        pkt[2] = 0x81;
        pkt[3] = 0x83;
        pkt[8..10].copy_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&[0xc0, HEADER_LEN as u8]);
        pkt.extend_from_slice(&TYPE_SOA.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&soa_ttl.to_be_bytes());
        // MNAME, RNAME, then the five 32-bit fields ending in MINIMUM.
        let mut rdata = vec![0u8, 0u8];
        rdata.extend_from_slice(&[0u8; 16]);
        rdata.extend_from_slice(&minimum.to_be_bytes());
        pkt.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&rdata);
        pkt
    }

    #[test]
    fn question_round_trips() {
        let pkt = query(0x1234, "example.com");
        let (q, end) = parse_question(&pkt).expect("a question");
        assert_eq!(q.qtype, 1);
        assert_eq!(q.qclass, 1);
        assert_eq!(q.name, b"\x07example\x03com\x00");
        assert_eq!(end, pkt.len());
        assert_eq!(id(&pkt), Some(0x1234));
    }

    #[test]
    fn a_name_is_keyed_case_insensitively() {
        let lower = parse_question(&query(1, "example.com")).unwrap().0;
        let upper = parse_question(&query(2, "EXAMPLE.CoM")).unwrap().0;
        assert_eq!(lower, upper);
    }

    #[test]
    fn a_truncated_message_is_refused_rather_than_guessed() {
        let pkt = query(1, "example.com");
        for cut in 0..pkt.len() {
            // Every prefix is either rejected or parsed; none may panic.
            let _ = parse_question(&pkt[..cut]);
        }
        assert!(parse_question(&pkt[..HEADER_LEN + 2]).is_none());
    }

    #[test]
    fn a_multi_question_message_is_not_cacheable() {
        let mut pkt = query(1, "example.com");
        pkt[4..6].copy_from_slice(&2u16.to_be_bytes());
        assert!(parse_question(&pkt).is_none());
    }

    #[test]
    fn the_message_ttl_is_its_shortest_record() {
        let pkt = response(1, "example.com", &[300, 60, 900]);
        assert_eq!(positive_ttl(&pkt), Some(60));
    }

    #[test]
    fn a_ttl_is_clamped_at_the_top_only() {
        let pkt = response(1, "example.com", &[u32::MAX]);
        assert_eq!(positive_ttl(&pkt), Some(TTL_CEILING));
        let pkt = response(1, "example.com", &[5]);
        assert_eq!(positive_ttl(&pkt), Some(5));
    }

    #[test]
    fn ttls_are_aged_in_place() {
        let mut pkt = response(1, "example.com", &[300, 60]);
        decrement_ttls(&mut pkt, 30);
        assert_eq!(positive_ttl(&pkt), Some(30));
        decrement_ttls(&mut pkt, 999);
        assert_eq!(positive_ttl(&pkt), Some(0));
    }

    #[test]
    fn an_opt_record_is_not_aged() {
        let mut pkt = response(1, "example.com", &[300]);
        pkt[10..12].copy_from_slice(&1u16.to_be_bytes());
        let opt_flags: u32 = 0x0000_8000;
        pkt.extend_from_slice(&[0]);
        pkt.extend_from_slice(&TYPE_OPT.to_be_bytes());
        pkt.extend_from_slice(&512u16.to_be_bytes());
        pkt.extend_from_slice(&opt_flags.to_be_bytes());
        pkt.extend_from_slice(&0u16.to_be_bytes());
        let opt_ttl_at = pkt.len() - 6;

        assert_eq!(positive_ttl(&pkt), Some(300));
        decrement_ttls(&mut pkt, 100);
        assert_eq!(u32_at(&pkt, opt_ttl_at), Some(opt_flags));
    }

    #[test]
    fn a_negative_answer_expires_on_the_soa() {
        let pkt = nxdomain(1, "nope.example.com", 3600, 90);
        assert_eq!(rcode(&pkt), 3);
        assert_eq!(negative_ttl(&pkt), Some(90));

        let pkt = nxdomain(1, "nope.example.com", 45, 3600);
        assert_eq!(negative_ttl(&pkt), Some(45));

        let pkt = nxdomain(1, "nope.example.com", 99_999, 99_999);
        assert_eq!(negative_ttl(&pkt), Some(NEGATIVE_TTL_CEILING));
    }

    #[test]
    fn a_positive_answer_has_no_negative_ttl() {
        let pkt = response(1, "example.com", &[300]);
        assert_eq!(negative_ttl(&pkt), None);
    }

    #[test]
    fn an_id_is_rewritten_for_the_client_that_asked() {
        let mut pkt = response(0xaaaa, "example.com", &[300]);
        set_id(&mut pkt, 0x5678);
        assert_eq!(id(&pkt), Some(0x5678));
    }

    #[test]
    fn a_hit_comes_back_aged() {
        let mut cache = Cache::new(8);
        let (q, _) = parse_question(&query(1, "example.com")).unwrap();
        cache.insert(q.clone(), response(1, "example.com", &[300]), 1_000, 300);

        match cache.get(&q, 1_030) {
            Lookup::Fresh(bytes) => assert_eq!(positive_ttl(&bytes), Some(270)),
            other => panic!("expected a fresh answer, got {other:?}"),
        }
        assert_eq!(cache.hits, 1);
    }

    #[test]
    fn an_expired_entry_is_stale_rather_than_fresh() {
        let mut cache = Cache::new(8);
        let (q, _) = parse_question(&query(1, "example.com")).unwrap();
        cache.insert(q.clone(), response(1, "example.com", &[60]), 1_000, 60);

        assert!(matches!(cache.get(&q, 1_059), Lookup::Fresh(_)));
        match cache.get(&q, 1_061) {
            Lookup::Stale(bytes) => assert_eq!(positive_ttl(&bytes), Some(0)),
            other => panic!("expected a stale answer, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_ttl_answer_is_not_stored() {
        let mut cache = Cache::new(8);
        let (q, _) = parse_question(&query(1, "example.com")).unwrap();
        cache.insert(q.clone(), response(1, "example.com", &[0]), 1_000, 0);
        assert_eq!(cache.get(&q, 1_000), Lookup::Miss);
    }

    #[test]
    fn the_least_recently_used_entry_is_the_one_evicted() {
        let mut cache = Cache::new(2);
        let names = ["a.test", "b.test", "c.test"];
        let qs: Vec<Question> = names
            .iter()
            .map(|n| parse_question(&query(1, n)).unwrap().0)
            .collect();

        cache.insert(qs[0].clone(), response(1, names[0], &[300]), 0, 300);
        cache.insert(qs[1].clone(), response(1, names[1], &[300]), 0, 300);
        // Touch the first, so the second is the stale one when the third lands.
        assert!(matches!(cache.get(&qs[0], 1), Lookup::Fresh(_)));
        cache.insert(qs[2].clone(), response(1, names[2], &[300]), 1, 300);

        assert_eq!(cache.len(), 2);
        assert!(matches!(cache.get(&qs[0], 2), Lookup::Fresh(_)));
        assert_eq!(cache.get(&qs[1], 2), Lookup::Miss);
        assert!(matches!(cache.get(&qs[2], 2), Lookup::Fresh(_)));
    }

    #[test]
    fn a_flush_empties_the_cache_but_keeps_the_counters() {
        let mut cache = Cache::new(8);
        let (q, _) = parse_question(&query(1, "example.com")).unwrap();
        cache.insert(q.clone(), response(1, "example.com", &[300]), 0, 300);
        assert!(matches!(cache.get(&q, 0), Lookup::Fresh(_)));

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get(&q, 0), Lookup::Miss);
        assert_eq!(cache.hits, 1);
        assert_eq!(cache.misses, 1);
    }
}
