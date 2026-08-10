//! Print the date and time.
//!
//! Local time comes from `TZ`, a fixed ISO 8601 offset the session carries;
//! there is no zone database, so the zone prints as that offset.

use std::env;

use edos_lib::time::{self, ClockTime};

const USAGE: &str = "usage: date [-u] [+FORMAT]\n\
    \x20 -u  print UTC instead of local time\n\
    Format directives: %Y %m %d %H %M %S %F %T %s %a %b %Z %n %t %%";

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn main() {
    let mut utc = false;
    let mut format: Option<String> = None;

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "-u" | "--utc" | "--universal" => utc = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            _ if arg.starts_with('+') => format = Some(arg[1..].to_string()),
            _ => {
                eprintln!("date: unrecognized argument: {arg}");
                eprintln!("{USAGE}");
                std::process::exit(1);
            }
        }
    }

    let Some(nanos) = time::clock_gettime_nanos() else {
        eprintln!("date: cannot read the clock");
        std::process::exit(1);
    };
    let unix_secs = (nanos / 1_000_000_000) as i64;
    let offset = if utc { 0 } else { time::utc_offset_seconds() };
    let t = ClockTime::from_unix_secs(unix_secs + offset as i64);

    match format {
        Some(f) => println!("{}", expand(&f, &t, unix_secs, offset, utc)),
        None => println!(
            "{} {} {:2} {:02}:{:02}:{:02} {} {}",
            WEEKDAYS[t.weekday as usize % 7],
            MONTHS[(t.month as usize - 1) % 12],
            t.day,
            t.hour,
            t.minute,
            t.second,
            zone_name(offset, utc),
            t.year
        ),
    }
}

/// `+02:00` for a local offset, `UTC` when the clock was asked for as UTC.
fn zone_name(offset: i32, utc: bool) -> String {
    if utc {
        return String::from("UTC");
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.abs();
    format!("{}{:02}:{:02}", sign, abs / 3_600, (abs % 3_600) / 60)
}

/// Substitute the supported `strftime` directives. An unknown directive is left
/// alone, `%` and all, so a typo is visible rather than silently dropped.
fn expand(format: &str, t: &ClockTime, unix_secs: i64, offset: i32, utc: bool) -> String {
    let mut out = String::with_capacity(format.len());
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&t.year.to_string()),
            Some('m') => out.push_str(&format!("{:02}", t.month)),
            Some('d') => out.push_str(&format!("{:02}", t.day)),
            Some('H') => out.push_str(&format!("{:02}", t.hour)),
            Some('M') => out.push_str(&format!("{:02}", t.minute)),
            Some('S') => out.push_str(&format!("{:02}", t.second)),
            Some('F') => out.push_str(&format!("{}-{:02}-{:02}", t.year, t.month, t.day)),
            Some('T') => out.push_str(&format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second)),
            Some('s') => out.push_str(&unix_secs.to_string()),
            Some('a') => out.push_str(WEEKDAYS[t.weekday as usize % 7]),
            Some('b') => out.push_str(MONTHS[(t.month as usize - 1) % 12]),
            Some('Z') => out.push_str(&zone_name(offset, utc)),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}
