//! Wall-clock time and sleeping.

use crate::{
    net,
    sys::{self, Errno},
};

/// POSIX `timespec`, laid out as the kernel reads it. Both a duration, for
/// [`nanosleep`], and an absolute time, for [`crate::io::utimensat`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// Sleep until at least `seconds` plus `nanos` have elapsed. A request that is
/// not a duration (`nanos` outside `0..1_000_000_000`, or a negative
/// `seconds`) is refused rather than clamped.
///
/// Unlike `std::thread::sleep`, which rounds the request down to whole
/// milliseconds, this honours the nanoseconds it is given. Nothing can cut the
/// sleep short, so there is no remaining time to read back.
pub fn nanosleep(seconds: i64, nanos: i64) -> Result<(), Errno> {
    let req = Timespec {
        tv_sec: seconds,
        tv_nsec: nanos,
    };
    sys::sys_ok(unsafe { sys::syscall2(sys::SYS_NANOSLEEP, &req as *const Timespec as u64, 0) })
}

/// Broken-down wall-clock time, in whichever zone the constructor was given.
#[derive(Debug, Clone, Copy)]
pub struct ClockTime {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub day: u8,
    pub month: u8,
    pub year: u16,
    /// Day of the week, 0 = Sunday.
    pub weekday: u8,
}

/// Nanoseconds since the Unix epoch, or `None` if the syscall fails.
///
/// The kernel samples the RTC once at boot and answers from its monotonic
/// counter, so this is cheap enough to call in a redraw loop.
pub fn clock_gettime_nanos() -> Option<u64> {
    let mut buf = [0u8; 8];
    let ret = unsafe { sys::syscall1(sys::SYS_CLOCK_GETTIME, buf.as_mut_ptr() as u64) };
    if ret == !0u64 {
        return None;
    }
    Some(u64::from_le_bytes(buf))
}

/// Step the wall clock to `nanos` nanoseconds since the Unix epoch.
///
/// The kernel reads the RTC once at boot, at one-second resolution, and counts
/// HPET ticks from there, so the clock starts up to a second wrong and drifts;
/// a time client calls this with what it learnt from the network. Only the wall
/// clock moves — monotonic time is unaffected. Returns `false` if the kernel
/// never sampled the RTC.
pub fn clock_settime_nanos(nanos: u64) -> bool {
    let ret = unsafe { sys::syscall1(sys::SYS_CLOCK_SETTIME, &nanos as *const u64 as u64) };
    ret != !0u64
}

/// Current UTC time broken down into date and time fields.
pub fn clock_gettime() -> Option<ClockTime> {
    clock_gettime_nanos().map(ClockTime::from_unix_nanos)
}

/// Current local time: UTC shifted by [`utc_offset_seconds`].
pub fn local_time() -> Option<ClockTime> {
    let secs = (clock_gettime_nanos()? / 1_000_000_000) as i64;
    Some(ClockTime::from_unix_secs(
        secs + utc_offset_seconds() as i64,
    ))
}

/// The session's offset from UTC, in seconds east of Greenwich.
///
/// Read from `TZ`, which holds a fixed ISO 8601 offset — `+02:00`, `-0530`,
/// `+02`, or `Z` — and not a POSIX zone rule or an IANA zone name. There is no
/// zone database, so a name means UTC rather than an offset. `edos-init` sets
/// the variable for the session; anything it cannot parse also means UTC.
pub fn utc_offset_seconds() -> i32 {
    std::env::var("TZ")
        .ok()
        .and_then(|tz| parse_utc_offset(&tz))
        .unwrap_or(0)
}

/// Seconds east of Greenwich for an ISO 8601 offset, or `None` if `s` is not one.
pub fn parse_utc_offset(s: &str) -> Option<i32> {
    let s = s.trim();
    if s.is_empty() || s == "Z" {
        return Some(0);
    }
    let (sign, rest) = match s.as_bytes()[0] {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (hh, mm) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None if rest.len() == 4 => (&rest[..2], &rest[2..]),
        None => (rest, "0"),
    };
    let hours: i32 = hh.parse().ok()?;
    let minutes: i32 = mm.parse().ok()?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 3_600 + minutes * 60))
}

impl ClockTime {
    /// Break nanoseconds since the Unix epoch down into UTC date and time.
    pub fn from_unix_nanos(nanos: u64) -> Self {
        Self::from_unix_secs((nanos / 1_000_000_000) as i64)
    }

    /// Break seconds since the Unix epoch down into date and time fields. The
    /// result is in whatever zone `secs` was already shifted into.
    pub fn from_unix_secs(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let secs_of_day = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Self {
            hour: (secs_of_day / 3_600) as u8,
            minute: ((secs_of_day % 3_600) / 60) as u8,
            second: (secs_of_day % 60) as u8,
            day,
            month,
            year,
            // 1970-01-01 was a Thursday, three days before Sunday.
            weekday: (days + 4).rem_euclid(7) as u8,
        }
    }
}

/// Proleptic Gregorian date for a count of days since 1970-01-01.
///
/// Howard Hinnant's `civil_from_days`:
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>
fn civil_from_days(days: i64) -> (u16, u8, u8) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as u16, m as u8, d as u8)
}

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch.
const NTP_UNIX_DELTA: u64 = 2_208_988_800;
/// The well-known NTP port.
pub const NTP_PORT: u16 = 123;
const NTP_PACKET_LEN: usize = 48;
/// Offset of the transmit timestamp within a packet, for both directions.
const NTP_TRANSMIT_OFFSET: usize = 40;

/// Nanoseconds since the Unix epoch at 2020-01-01, the point below which a
/// clock is treated as unset rather than merely wrong.
///
/// The kernel reads the RTC at boot, so a clock this far behind means the RTC
/// itself is unset. It matters because TLS checks certificate validity against
/// this clock, and an unset one rejects every certificate on earth.
pub const CLOCK_PLAUSIBLE_FROM_NANOS: u64 = 1_577_836_800 * 1_000_000_000;

/// What one NTP server answered.
pub struct SntpSample {
    /// Server time at the moment the reply was sent, in nanoseconds since the
    /// Unix epoch.
    pub server_nanos: u64,
    /// How far the local clock is behind the server, in nanoseconds.
    pub offset_nanos: i64,
    /// Round-trip delay less the server's own processing time, in nanoseconds.
    pub delay_nanos: i64,
    pub stratum: u8,
}

/// Send one SNTP client packet and turn the reply into a [`SntpSample`].
///
/// RFC 4330. One round trip, no state kept between calls.
pub fn sntp_query(ip: [u8; 4], port: u16, timeout_ms: u64) -> Result<SntpSample, String> {
    let fd = net::create_udp_socket().map_err(|_| "cannot create a UDP socket".to_string())?;
    if net::set_recv_timeout(fd, timeout_ms).is_err() {
        net::close(fd);
        return Err("cannot set a receive timeout".to_string());
    }

    let mut packet = [0u8; NTP_PACKET_LEN];
    // Leap indicator 0, version 4, mode 3 (client).
    packet[0] = 0x23;

    let t1 = sntp_now()?;
    packet[NTP_TRANSMIT_OFFSET..NTP_TRANSMIT_OFFSET + 8]
        .copy_from_slice(&unix_to_ntp(t1).to_be_bytes());

    let addr = net::SockAddrIn::new(ip, port);
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
    let t4 = sntp_now()?;

    if len < NTP_PACKET_LEN {
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
    let t3 = ntp_to_unix(ntp_at(&reply, NTP_TRANSMIT_OFFSET));
    if t3 == 0 {
        return Err("reply carries no transmit timestamp".to_string());
    }

    // RFC 4330 §5: the offset is the mean of the two one-way differences, and
    // the delay is the round trip less the time the server held the request.
    let offset_nanos = ((t2 as i64 - t1 as i64) + (t3 as i64 - t4 as i64)) / 2;
    let delay_nanos = (t4 as i64 - t1 as i64) - (t3 as i64 - t2 as i64);

    Ok(SntpSample {
        server_nanos: t3,
        offset_nanos,
        delay_nanos,
        stratum,
    })
}

/// Step the wall clock by the offset a sample measured.
pub fn sntp_step_clock(sample: &SntpSample) -> Result<(), String> {
    let local = clock_gettime_nanos().ok_or_else(|| "the kernel has no wall clock".to_string())?;
    let target = local.saturating_add_signed(sample.offset_nanos);
    if clock_settime_nanos(target) {
        Ok(())
    } else {
        Err("the kernel refused to set the clock".to_string())
    }
}

fn sntp_now() -> Result<u64, String> {
    clock_gettime_nanos().ok_or_else(|| "the kernel has no wall clock".to_string())
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
