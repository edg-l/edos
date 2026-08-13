//! An SNTP client: one UDP round trip to a time server, and optionally the
//! clock step it implies.
//!
//! RFC 4330. The kernel reads the RTC once at boot with one-second resolution
//! and counts HPET ticks from there, so the wall clock starts up to a second
//! wrong and drifts; this is what corrects it.

use std::{env, process};

use edos_lib::{
    net,
    time::{self, ClockTime, NTP_PORT, SntpSample},
};

struct Options {
    servers: Vec<String>,
    port: u16,
    timeout_ms: u64,
    set_clock: bool,
    utc: bool,
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
        match time::sntp_query(ip, opts.port, opts.timeout_ms) {
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

fn report(server: &str, ip: [u8; 4], sample: &SntpSample, opts: &Options) {
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

fn step_clock(sample: &SntpSample) {
    match time::sntp_step_clock(sample) {
        Ok(()) => println!("clock stepped by {} s", format_seconds(sample.offset_nanos)),
        Err(err) => eprintln!("sntp: {}", err),
    }
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
