use std::env;
use std::io::{self, Write};

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        // Remove trailing zeros after decimal point
        let s = format!("{}", n);
        s
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let (first, step, last) = match args.len() - 1 {
        1 => {
            let last: f64 = match args[1].parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("seq: invalid number: {}", args[1]);
                    std::process::exit(1);
                }
            };
            (1.0f64, 1.0f64, last)
        }
        2 => {
            let first: f64 = match args[1].parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("seq: invalid number: {}", args[1]);
                    std::process::exit(1);
                }
            };
            let last: f64 = match args[2].parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("seq: invalid number: {}", args[2]);
                    std::process::exit(1);
                }
            };
            (first, 1.0f64, last)
        }
        3 => {
            let first: f64 = match args[1].parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("seq: invalid number: {}", args[1]);
                    std::process::exit(1);
                }
            };
            let step: f64 = match args[2].parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("seq: invalid number: {}", args[2]);
                    std::process::exit(1);
                }
            };
            let last: f64 = match args[3].parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("seq: invalid number: {}", args[3]);
                    std::process::exit(1);
                }
            };
            (first, step, last)
        }
        _ => {
            eprintln!("usage: seq [first [increment]] last");
            std::process::exit(1);
        }
    };

    if step == 0.0 {
        eprintln!("seq: step cannot be zero");
        std::process::exit(1);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut n = first;
    while (step > 0.0 && n <= last) || (step < 0.0 && n >= last) {
        let line = format!("{}\n", format_num(n));
        if out.write_all(line.as_bytes()).is_err() {
            break;
        }
        n += step;
    }
}
