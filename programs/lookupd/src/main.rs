//! A caching name lookup daemon.
//!
//! Listens on `127.0.0.1:53`, answers from cache when it can, and forwards
//! upstream when it cannot. Clients need no change at all: they already ask
//! `SYS_GETDNS` for an address and send DNS to it, so pointing that at loopback
//! puts this in the path of every program on the machine.
//!
//! Design and rationale: `doc/design/lookupd.md`.

mod dns;

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use dns::{Cache, Lookup, Question};
use edos_lib::{
    config,
    net::{self, SockAddrIn},
    process::{self, SIGHUP},
};

/// The address the daemon claims, and the one every client is redirected to.
const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
const DNS_PORT: u16 = 53;

/// One value: the upstream resolver's address, or `dhcp` to use whatever the
/// lease offered. Its absence means the service is not configured at all, which
/// is what init's `enabled_by` tests before starting this at all.
const CONFIG: &str = "/etc/lookupd.conf";

/// Where the counters are published, in the shape of a `/proc` file.
///
/// `/tmp` is memfs and forgets at reboot, which is right: this is a record of
/// what this boot's resolver has done, not a setting. The kernel's own
/// counters live in procfs, and a userspace daemon has no entry there.
const STATS: &str = "/tmp/lookupd.stats";

/// The counters are rewritten no more often than this, so a burst of queries
/// costs one write rather than one per query.
const STATS_INTERVAL_SECS: u64 = 1;

/// How long a client's datagram is waited for before the loop goes round again.
/// Short enough that a `SIGHUP` takes effect promptly, since a handler runs
/// when the process next returns from a syscall.
const POLL_MS: u64 = 1_000;

/// How long an upstream answer is waited for, and how many times a query is
/// sent. UDP drops datagrams, and the first one to a new peer routinely is:
/// the send that triggers ARP resolution is discarded while the reply arrives.
const UPSTREAM_MS: u64 = 2_000;
const UPSTREAM_ATTEMPTS: u32 = 2;

/// Set by the `SIGHUP` handler and acted on by the loop. A handler must not
/// touch the cache itself: it runs on the process's own stack, between any two
/// instructions the loop was in the middle of.
static FLUSH_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_hup(_signum: u32) {
    FLUSH_REQUESTED.store(true, Ordering::SeqCst);
}

/// One line to `/dev/klog`, built before it is written.
///
/// `writeln!` issues a write per fragment and the log interleaves them with
/// everything else on the machine, so a formatted line arrives in pieces.
fn log(line: &str) {
    if let Ok(mut klog) = OpenOptions::new().write(true).open("/dev/klog") {
        let _ = klog.write_all(format!("lookupd: {line}\n").as_bytes());
    }
}

/// Publish the counters, in the one-key-per-line shape `/proc` files use.
fn write_stats(cache: &Cache, upstream: [u8; 4]) {
    let [a, b, c, d] = upstream;
    let text = format!(
        "upstream: {a}.{b}.{c}.{d}\nentries: {}\nhits: {}\nmisses: {}\nstale: {}\n",
        cache.len(),
        cache.hits,
        cache.misses,
        cache.stale_served,
    );
    let _ = std::fs::write(STATS, text);
}

/// The address DHCP learned, read from `/proc/net` rather than `SYS_GETDNS`.
///
/// This is the loop trap, and it is the first thing to get wrong: once the
/// override below is installed, `get_dns` answers `127.0.0.1`, so a daemon that
/// asked the kernel for its own upstream would query itself forever. `/proc/net`
/// reports the DHCP-learned address whatever the override says.
fn dhcp_resolver() -> Option<[u8; 4]> {
    let text = std::fs::read_to_string("/proc/net").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("dns:"))
        .and_then(|v| net::parse_ipv4(v.trim()))
}

/// Where queries this cache cannot answer are sent.
fn upstream() -> Option<[u8; 4]> {
    match config::read(CONFIG).as_deref() {
        Some("dhcp") | None => dhcp_resolver(),
        Some(value) => net::parse_ipv4(value).or_else(|| {
            log(&format!(
                "{CONFIG}: '{value}' is not an address, using DHCP's"
            ));
            dhcp_resolver()
        }),
    }
}

/// A response to `query` carrying nothing but a failure code.
///
/// RCODE 2 is SERVFAIL (RFC 1035 section 4.1.1), which is what a client should
/// see when the upstream could not be reached and nothing stale was held.
fn servfail(query: &[u8]) -> Vec<u8> {
    let mut out = query.to_vec();
    if out.len() < dns::HEADER_LEN {
        return out;
    }
    out[2] = 0x81;
    out[3] = 0x82;
    out[6..12].fill(0);
    out.truncate(
        dns::parse_question(query)
            .map(|(_, end)| end)
            .unwrap_or(dns::HEADER_LEN),
    );
    out
}

struct Upstream {
    fd: u64,
    addr: SockAddrIn,
    next_id: u16,
}

impl Upstream {
    fn new(ip: [u8; 4]) -> Option<Self> {
        let fd = net::create_udp_socket().ok()?;
        net::set_recv_timeout(fd, UPSTREAM_MS).ok()?;
        Some(Self {
            fd,
            addr: SockAddrIn::new(ip, DNS_PORT),
            next_id: 1,
        })
    }

    /// Ask the upstream `query`, and return its answer.
    ///
    /// The query goes out under an id of this daemon's choosing rather than the
    /// client's, so a late answer to an earlier question cannot be mistaken for
    /// this one; the caller stamps the client's id back on.
    fn ask(&mut self, query: &[u8], question: Option<&Question>) -> Option<Vec<u8>> {
        let mut outgoing = query.to_vec();
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        dns::set_id(&mut outgoing, id);

        for _ in 0..UPSTREAM_ATTEMPTS {
            if net::sendto(self.fd, &outgoing, Some(&self.addr)).is_err() {
                continue;
            }
            let mut buf = [0u8; dns::MAX_MESSAGE];
            let Ok(n) = net::recvfrom(self.fd, &mut buf) else {
                continue;
            };
            let answer = &buf[..n];
            if dns::id(answer) != Some(id) {
                continue;
            }
            // A server that answered a different question than the one asked is
            // not answering this query, whatever its id says.
            if let Some(want) = question
                && dns::parse_question(answer).map(|(q, _)| q).as_ref() != Some(want)
            {
                continue;
            }
            return Some(answer.to_vec());
        }
        None
    }
}

fn main() {
    let Some(upstream_ip) = upstream() else {
        log("no upstream resolver: /proc/net has no dns line and the config names none");
        std::process::exit(1);
    };

    let Ok(listener) = net::create_udp_socket() else {
        log("could not create the listening socket");
        std::process::exit(1);
    };
    if net::bind(listener, &SockAddrIn::new(LOOPBACK, DNS_PORT)).is_err() {
        log("could not bind 127.0.0.1:53; is another resolver running?");
        std::process::exit(1);
    }
    if net::set_recv_timeout(listener, POLL_MS).is_err() {
        log("could not set a receive timeout on the listening socket");
        std::process::exit(1);
    }

    let Some(mut server) = Upstream::new(upstream_ip) else {
        log("could not create the upstream socket");
        std::process::exit(1);
    };

    // The override goes in only once the socket is bound, so there is no window
    // in which a client is pointed at a port nothing is listening on.
    if net::set_dns(LOOPBACK).is_err() {
        log("could not install the resolver override");
        std::process::exit(1);
    }
    let _ = process::signal(SIGHUP, on_hup);

    let [a, b, c, d] = upstream_ip;
    log(&format!(
        "listening on 127.0.0.1:53, upstream {a}.{b}.{c}.{d}"
    ));

    let started = Instant::now();
    let mut cache = Cache::new(dns::MAX_ENTRIES);
    let mut buf = [0u8; dns::MAX_MESSAGE];
    let mut stats_written_at = 0u64;
    write_stats(&cache, upstream_ip);

    loop {
        let now = started.elapsed().as_secs();

        if FLUSH_REQUESTED.swap(false, Ordering::SeqCst) {
            let entries = cache.len();
            cache.clear();
            log(&format!(
                "flushed {entries} entries; hits {} misses {} stale {}",
                cache.hits, cache.misses, cache.stale_served
            ));
            write_stats(&cache, upstream_ip);
            stats_written_at = now;
        }

        // Published from here rather than after answering, so the receive
        // timeout refreshes them too: a burst of queries that then stops must
        // not leave the last one uncounted until the next one arrives.
        if now.saturating_sub(stats_written_at) >= STATS_INTERVAL_SECS {
            write_stats(&cache, upstream_ip);
            stats_written_at = now;

            // The override is a single slot: whoever writes it last owns it,
            // and a process that installed its own and exited leaves the
            // system pointed back at the upstream. Claiming it again here is
            // what makes that self-healing rather than permanent.
            if net::get_dns() != Some(LOOPBACK) && net::set_dns(LOOPBACK).is_ok() {
                log("reclaimed the resolver override");
            }
        }

        let mut from = SockAddrIn::new([0; 4], 0);
        let mut from_len = core::mem::size_of::<SockAddrIn>() as u32;
        let Ok(n) = net::recvfrom_flags(listener, &mut buf, 0, Some((&mut from, &mut from_len)))
        else {
            continue;
        };
        if n < dns::HEADER_LEN {
            continue;
        }
        let query = &buf[..n];

        let reply = match dns::parse_question(query) {
            Some((question, _)) => answer(&mut cache, &mut server, query, &question, now),
            // Anything this cache cannot key is forwarded verbatim, so a client
            // asking something unusual is passed through rather than refused.
            None => server
                .ask(query, None)
                .map(|mut bytes| {
                    dns::set_id(&mut bytes, dns::id(query).unwrap_or(0));
                    bytes
                })
                .unwrap_or_else(|| servfail(query)),
        };

        let _ = net::sendto(listener, &reply, Some(&from));
    }
}

/// Answer one query, from the cache when it can and upstream when it cannot.
fn answer(
    cache: &mut Cache,
    server: &mut Upstream,
    query: &[u8],
    question: &Question,
    now: u64,
) -> Vec<u8> {
    let client_id = dns::id(query).unwrap_or(0);

    let held = cache.get(question, now);
    if let Lookup::Fresh(mut bytes) = held {
        dns::set_id(&mut bytes, client_id);
        return bytes;
    }

    match server.ask(query, Some(question)) {
        Some(mut fresh) => {
            // A truncated answer is passed through so the client sees exactly
            // what it would have seen without this daemon in the way, and is
            // not cached: what it holds is incomplete by definition.
            if !dns::truncated(&fresh) {
                let ttl = dns::positive_ttl(&fresh).or_else(|| dns::negative_ttl(&fresh));
                if let Some(ttl) = ttl {
                    cache.insert(question.clone(), fresh.clone(), now, ttl);
                }
            }
            dns::set_id(&mut fresh, client_id);
            fresh
        }
        // RFC 8767: an expired answer beats no answer when the upstream is
        // unreachable, which on this machine it regularly is.
        None => match held {
            Lookup::Stale(mut bytes) => {
                cache.count_stale_served();
                dns::set_id(&mut bytes, client_id);
                bytes
            }
            _ => servfail(query),
        },
    }
}
