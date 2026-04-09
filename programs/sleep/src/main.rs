use std::{env, thread, time::Duration};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: sleep <seconds>");
        std::process::exit(1);
    }

    let secs: f64 = match args[1].parse() {
        Ok(s) => s,
        Err(_) => {
            eprintln!("sleep: invalid number: {}", args[1]);
            std::process::exit(1);
        }
    };

    let millis = (secs * 1000.0) as u64;
    thread::sleep(Duration::from_millis(millis));
}
