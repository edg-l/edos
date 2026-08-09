//! Wall-clock time and sleeping.

use crate::sys;

/// POSIX `timespec`, laid out as the kernel reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// Sleep until at least `seconds` plus `nanos` have elapsed. Returns 0, or -1
/// if the request is not a duration (`nanos` outside `0..1_000_000_000`, or a
/// negative `seconds`).
///
/// Unlike `std::thread::sleep`, which rounds the request down to whole
/// milliseconds, this honours the nanoseconds it is given. Nothing can cut the
/// sleep short, so there is no remaining time to read back.
pub fn nanosleep(seconds: i64, nanos: i64) -> i64 {
    let req = Timespec {
        tv_sec: seconds,
        tv_nsec: nanos,
    };
    unsafe { sys::syscall2(sys::SYS_NANOSLEEP, &req as *const Timespec as u64, 0) as i64 }
}

/// Broken-down UTC wall-clock time.
#[derive(Debug, Clone, Copy)]
pub struct ClockTime {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub day: u8,
    pub month: u8,
    pub year: u16,
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

/// Current UTC time broken down into date and time fields.
pub fn clock_gettime() -> Option<ClockTime> {
    clock_gettime_nanos().map(ClockTime::from_unix_nanos)
}

impl ClockTime {
    /// Break nanoseconds since the Unix epoch down into UTC date and time.
    pub fn from_unix_nanos(nanos: u64) -> Self {
        let secs = (nanos / 1_000_000_000) as i64;
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
