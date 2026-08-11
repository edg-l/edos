//! An SNTP client: one UDP round trip to a time server, and optionally the
//! clock step it implies.
//!
//! RFC 4330. The kernel reads the RTC once at boot with one-second resolution
//! and counts HPET ticks from there, so the wall clock starts up to a second
//! wrong and drifts; this is what corrects it.

use std::{env, process};

use edos_lib::{
    net::{self, SockAddrIn},
    time::{self, ClockTime},
};

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch.
const NTP_UNIX_DELTA: u64 = 2_208_988_800;
const NTP_PORT: u16 = 123;
const PACKET_LEN: usize = 48;
/// Offset of the transmit timestamp within a packet, for both directions.
const TRANSMIT_OFFSET: usize = 40;

struct Options {
    servers: Vec<String>,
    port: u16,
    timeout_ms: u64,
    set_clock: bool,
    utc: bool,
}

/// What one server answered.
struct Sample {
    /// Server time at the moment the reply was sent, in nanoseconds since the
    /// Unix epoch.
    server_nanos: u64,
    /// How far the local clock is behind the server, in nanoseconds.
    offset_nanos: i64,
    /// Round-trip delay less the server's own processing time, in nanoseconds.
    delay_nanos: i64,
    stratum: u8,
}

fn main() {
    let opts = parse_args();

    let mut stepped = false;
    let mut failures = 0;

    for server in &opts.servers {
        let Some(ip) = net::resolve_host(server) else {
            eprintln!("sntp: cannot resolve {}", server);
            failures += 1;
            continue;
        };
        match query(ip, opts.port, opts.timeout_ms) {
            Ok(sample) => {
                report(server, ip, &sample, &opts);
                if opts.set_clock && !stepped {
                    step_clock(&sample);
                    stepped = true;
                }
            }
            Err(err) => {
                eprintln!("sntp: {} ({}): {}", server, format_ip(ip), err);
                failures += 1;
            }
        }
    }

    if failures == opts.servers.len() {
        process::exit(1);
    }
}

/// Send one client packet and turn the reply into a `Sample`.
fn query(ip: [u8; 4], port: u16, timeout_ms: u64) -> Result<Sample, String> {
    let fd = net::create_udp_socket().map_err(|_| "cannot create a UDP socket".to_string())?;
    if net::set_recv_timeout(fd, timeout_ms).is_err() {
        net::close(fd);
        return Err("cannot set a receive timeout".to_string());
    }

    let mut packet = [0u8; PACKET_LEN];
    // Leap indicator 0, version 4, mode 3 (client).
    packet[0] = 0x23;

    let t1 = now()?;
    packet[TRANSMIT_OFFSET..TRANSMIT_OFFSET + 8].copy_from_slice(&unix_to_ntp(t1).to_be_bytes());

    let addr = SockAddrIn::new(ip, port);
    if net::sendto(fd, &packet, Some(&addr)).is_err() {
        net::close(fd);
        return Err("send failed".to_string());
    }

    let mut reply = [0u8; 128];
    let received = net::recvfrom(fd, &mut reply);
    net::close(fd);
    let len = received.map_err(|_| "no reply".to_string())?;
    // The local timestamp belongs immediately after the read, before any
    // parsing work is charged to the round trip.
    let t4 = now()?;

    if len < PACKET_LEN {
        return Err(format!("short reply: {} bytes", len));
    }
    let mode = reply[0] & 0x07;
    if mode != 4 {
        return Err(format!("reply is not from a server (mode {})", mode));
    }
    let stratum = reply[1];
    if stratum == 0 {
        let code = String::from_utf8_lossy(&reply[12..16])
            .trim_end()
            .to_string();
        return Err(format!("kiss-o'-death: {}", code));
    }
    if stratum > 15 {
        return Err(format!("server is unsynchronised (stratum {})", stratum));
    }

    let originate = ntp_at(&reply, 24);
    if originate != unix_to_ntp(t1) {
        return Err("reply does not echo the transmit timestamp".to_string());
    }
    let t2 = ntp_to_unix(ntp_at(&reply, 32));
    let t3 = ntp_to_unix(ntp_at(&reply, TRANSMIT_OFFSET));
    if t3 == 0 {
        return Err("reply carries no transmit timestamp".to_string());
    }

    // RFC 4330 §5: the offset is the mean of the two one-way differences, and
    // the delay is the round trip less the time the server held the request.
    let offset_nanos = ((t2 as i64 - t1 as i64) + (t3 as i64 - t4 as i64)) / 2;
    let delay_nanos = (t4 as i64 - t1 as i64) - (t3 as i64 - t2 as i64);

    Ok(Sample {
        server_nanos: t3,
        offset_nanos,
        delay_nanos,
        stratum,
    })
}

fn report(server: &str, ip: [u8; 4], sample: &Sample, opts: &Options) {
    let shown = if opts.utc {
        sample.server_nanos
    } else {
        let shift = time::utc_offset_seconds() as i64 * 1_000_000_000;
        sample.server_nanos.saturating_add_signed(shift)
    };
    let zone = if opts.utc {
        "UTC".to_string()
    } else {
        format_offset(time::utc_offset_seconds())
    };

    println!(
        "{} {} ({}) stratum {} {}",
        format_time(shown),
        zone,
        format_ip(ip),
        sample.stratum,
        stratum_name(sample.stratum),
    );
    println!(
        "offset {} s, delay {} s, from {}",
        format_seconds(sample.offset_nanos),
        format_seconds(sample.delay_nanos.max(0)),
        server,
    );
}

fn step_clock(sample: &Sample) {
    let Some(local) = time::clock_gettime_nanos() else {
        eprintln!("sntp: the kernel has no wall clock");
        return;
    };
    let target = local.saturating_add_signed(sample.offset_nanos);
    if time::clock_settime_nanos(target) {
        println!("clock stepped by {} s", format_seconds(sample.offset_nanos));
    } else {
        eprintln!("sntp: the kernel refused to set the clock");
    }
}

fn now() -> Result<u64, String> {
    time::clock_gettime_nanos().ok_or_else(|| "the kernel has no wall clock".to_string())
}

/// Read a big-endian 64-bit NTP timestamp out of a packet.
fn ntp_at(packet: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&packet[offset..offset + 8]);
    u64::from_be_bytes(bytes)
}

/// Nanoseconds since the Unix epoch for an NTP timestamp, or 0 if it is unset.
///
/// NTP seconds are a 32-bit count that wraps in 2036. A value below the Unix
/// epoch is therefore not a date in 1900 but one in the next era, which is the
/// interpretation RFC 4330 §3 requires.
fn ntp_to_unix(ts: u64) -> u64 {
    if ts == 0 {
        return 0;
    }
    let seconds = ts >> 32;
    let fraction = ts & 0xffff_ffff;
    let unix_seconds = if seconds >= NTP_UNIX_DELTA {
        seconds - NTP_UNIX_DELTA
    } else {
        seconds + (1u64 << 32) - NTP_UNIX_DELTA
    };
    unix_seconds * 1_000_000_000 + ((fraction * 1_000_000_000) >> 32)
}

/// An NTP timestamp for nanoseconds since the Unix epoch.
fn unix_to_ntp(nanos: u64) -> u64 {
    let seconds = (nanos / 1_000_000_000) + NTP_UNIX_DELTA;
    let fraction = ((nanos % 1_000_000_000) << 32) / 1_000_000_000;
    ((seconds & 0xffff_ffff) << 32) | fraction
}

fn stratum_name(stratum: u8) -> &'static str {
    match stratum {
        1 => "(primary)",
        _ => "(secondary)",
    }
}

fn format_ip(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

fn format_time(nanos: u64) -> String {
    let t = ClockTime::from_unix_nanos(nanos);
    let millis = (nanos % 1_000_000_000) / 1_000_000;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.year, t.month, t.day, t.hour, t.minute, t.second, millis
    )
}

/// A signed offset in seconds, to microsecond resolution.
fn format_seconds(nanos: i64) -> String {
    let sign = if nanos < 0 { '-' } else { '+' };
    let magnitude = nanos.unsigned_abs();
    format!(
        "{}{}.{:06}",
        sign,
        magnitude / 1_000_000_000,
        (magnitude % 1_000_000_000) / 1_000
    )
}

/// An ISO 8601 zone offset, the form `TZ` itself takes.
fn format_offset(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let magnitude = seconds.unsigned_abs();
    format!(
        "{}{:02}:{:02}",
        sign,
        magnitude / 3_600,
        (magnitude % 3_600) / 60
    )
}

fn parse_args() -> Options {
    let mut opts = Options {
        servers: Vec::new(),
        port: NTP_PORT,
        timeout_ms: 5_000,
        set_clock: false,
        utc: false,
    };

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                usage();
                process::exit(0);
            }
            "-s" | "--set" => opts.set_clock = true,
            "-u" | "--utc" => opts.utc = true,
            "-p" | "--port" => match args.next().and_then(|v| v.parse().ok()) {
                Some(port) => opts.port = port,
                None => fail("-p wants a port number"),
            },
            "-t" | "--timeout" => match args.next().and_then(|v| v.parse::<f64>().ok()) {
                Some(secs) if secs > 0.0 => opts.timeout_ms = (secs * 1000.0) as u64,
                _ => fail("-t wants a positive number of seconds"),
            },
            _ if arg.starts_with('-') && arg.len() > 1 => {
                fail(&format!("unknown option: {}", arg));
            }
            _ => opts.servers.push(arg),
        }
    }

    if opts.servers.is_empty() {
        opts.servers.push("pool.ntp.org".to_string());
    }
    opts
}

fn fail(message: &str) -> ! {
    eprintln!("sntp: {}", message);
    usage();
    process::exit(2);
}

fn usage() {
    eprintln!("usage: sntp [-s] [-u] [-t SECONDS] [-p PORT] [SERVER...]");
    eprintln!("  -s  step the system clock to the first server that answers");
    eprintln!("  -u  report UTC rather than local time");
    eprintln!("  -t  give up on a reply after SECONDS (default 5)");
    eprintln!("  -p  query PORT rather than 123");
    eprintln!("SERVER defaults to pool.ntp.org.");
}
