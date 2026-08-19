use std::env;

fn main() {
    let now = current_date();

    let args: Vec<String> = env::args().collect();
    let (month, year) = match args.len() {
        1 => (now.1, now.2),
        2 => {
            let y: u32 = match args[1].parse() {
                Ok(y) => y,
                Err(_) => {
                    eprintln!("cal: invalid year: {}", args[1]);
                    std::process::exit(1);
                }
            };
            (0, y)
        }
        3 => {
            let m: u32 = match args[1].parse() {
                Ok(m) if (1..=12).contains(&m) => m,
                _ => {
                    eprintln!("cal: invalid month: {}", args[1]);
                    std::process::exit(1);
                }
            };
            let y: u32 = match args[2].parse() {
                Ok(y) => y,
                Err(_) => {
                    eprintln!("cal: invalid year: {}", args[2]);
                    std::process::exit(1);
                }
            };
            (m, y)
        }
        _ => {
            eprintln!("usage: cal [[month] year]");
            std::process::exit(1);
        }
    };

    if month == 0 {
        print_year_calendar(year, &now);
    } else {
        print_month_calendar(month, year, &now);
    }
}

fn is_leap(y: u32) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

fn month_lengths(y: u32) -> [u32; 12] {
    [
        31,
        if is_leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ]
}

fn day_of_week(y: u32, m: u32, d: u32) -> usize {
    let (y, m) = if m < 3 { (y - 1, m + 12) } else { (y, m) };
    let y = y as usize;
    let m = m as usize;
    let d = d as usize;
    // Zeller returns 0=Saturday; shift so 0=Sunday to match the "Su Mo Tu We Th Fr Sa" header.
    ((d + (13 * (m + 1)) / 5 + y + y / 4 - y / 100 + y / 400) % 7 + 6) % 7
}

/// Today, as (day, month, year) in the session's local time.
fn current_date() -> (u32, u32, u32) {
    match edos_lib::time::local_time() {
        Some(t) => (t.day as u32, t.month as u32, t.year as u32),
        None => (1, 1, 1970),
    }
}

/// Whether `(d, month, year)` is _today_ (according to the clock).
fn is_today(d: u32, month: u32, year: u32, now: &(u32, u32, u32)) -> bool {
    d == now.0 && month == now.1 && year == now.2
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn print_month_calendar(month: u32, year: u32, now: &(u32, u32, u32)) {
    let header = format!("{} {}", MONTH_NAMES[(month - 1) as usize], year);
    let pad = (20usize.saturating_sub(header.len())) / 2;
    println!("{:>pad$}{}", "", header);
    println!("Su Mo Tu We Th Fr Sa");

    let days_in_month = month_lengths(year)[(month - 1) as usize];
    let start_dow = day_of_week(year, month, 1);

    for _ in 0..start_dow {
        print!("   ");
    }

    for day in 1..=days_in_month {
        if is_today(day, month, year, now) {
            print!("\x1b[107;30m{:2}\x1b[0m ", day);
        } else {
            print!("{:2} ", day);
        }
        if (start_dow + day as usize).is_multiple_of(7) {
            println!();
        }
    }
    if !(start_dow + days_in_month as usize).is_multiple_of(7) {
        println!();
    }
}

fn print_year_calendar(year: u32, now: &(u32, u32, u32)) {
    // 2 header lines (month name + weekday header) + up to 6 week rows.
    const MONTH_BLOCK_LINES: usize = 8;
    println!("{:>30}", year);
    println!();
    for row in 0..4 {
        let mut lines: Vec<String> = vec![String::new(); MONTH_BLOCK_LINES];
        for col in 0..3 {
            let month = (row * 3 + col + 1) as u32;
            let name = MONTH_NAMES[(month - 1) as usize];
            let header = name.to_string();
            let pad = (20usize.saturating_sub(header.len())) / 2;
            lines[0].push_str(&format!("{:>pad$}{}", "", header));
            lines[0].push_str("   ");

            let hdr2 = "Su Mo Tu We Th Fr Sa";
            lines[1].push_str(hdr2);
            lines[1].push_str("   ");

            let days_in_month = month_lengths(year)[(month - 1) as usize];
            let start_dow = day_of_week(year, month, 1);

            let mut cur_line = 2;
            let mut cur_pos = 0;
            for _ in 0..start_dow {
                lines[cur_line].push_str("   ");
                cur_pos += 1;
            }
            for day in 1..=days_in_month {
                if is_today(day, month, year, now) {
                    lines[cur_line].push_str(&format!("\x1b[107;30m{:2}\x1b[0m ", day));
                } else {
                    lines[cur_line].push_str(&format!("{:2} ", day));
                }
                cur_pos += 1;
                if cur_pos == 7 {
                    lines[cur_line].push_str("  ");
                    cur_pos = 0;
                    cur_line += 1;
                }
            }
            if cur_pos != 0 {
                lines[cur_line].push_str("  ");
                cur_line += 1;
            }
            while cur_line < MONTH_BLOCK_LINES {
                lines[cur_line].push_str(&" ".repeat(21));
                lines[cur_line].push_str("  ");
                cur_line += 1;
            }
        }
        for line in &lines {
            println!("{}", line);
        }
    }
}
