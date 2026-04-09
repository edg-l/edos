use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: ping <ip>");
        return;
    }

    let ip = match resolve_host(&args[1]) {
        Some(ip) => ip,
        None => {
            eprintln!("Invalid IP address: {}", args[1]);
            return;
        }
    };

    println!("PING {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);

    let id = std::process::id() as u16;
    let mut sent = 0u32;
    let mut received = 0u32;
    let mut min_rtt = u64::MAX;
    let mut max_rtt = 0u64;
    let mut total_rtt = 0u64;

    for seq in 0..4u16 {
        sent += 1;
        match edos_lib::net::ping(ip, id, seq, 5000) {
            Some(rtt_us) => {
                received += 1;
                let ms_whole = rtt_us / 1000;
                let ms_frac = (rtt_us % 1000) / 10;
                println!(
                    "Reply from {}.{}.{}.{}: seq={} time={}.{:02}ms",
                    ip[0], ip[1], ip[2], ip[3], seq, ms_whole, ms_frac
                );
                if rtt_us < min_rtt {
                    min_rtt = rtt_us;
                }
                if rtt_us > max_rtt {
                    max_rtt = rtt_us;
                }
                total_rtt += rtt_us;
            }
            None => {
                println!("Request timeout for seq {}", seq);
            }
        }

        if seq < 3 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    println!("--- ping statistics ---");
    println!("{} packets sent, {} received", sent, received);
    if received > 0 {
        let avg = total_rtt / received as u64;
        let avg_whole = avg / 1000;
        let avg_frac = (avg % 1000) / 10;
        let min_whole = min_rtt / 1000;
        let min_frac = (min_rtt % 1000) / 10;
        let max_whole = max_rtt / 1000;
        let max_frac = (max_rtt % 1000) / 10;
        println!(
            "rtt min/avg/max = {}.{:02}/{}.{:02}/{}.{:02} ms",
            min_whole, min_frac, avg_whole, avg_frac, max_whole, max_frac
        );
    }
}

fn resolve_host(s: &str) -> Option<[u8; 4]> {
    if s == "localhost" {
        return Some([127, 0, 0, 1]);
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() == 4 {
        let mut ip = [0u8; 4];
        let mut ok = true;
        for (i, part) in parts.iter().enumerate() {
            match part.parse() {
                Ok(v) => ip[i] = v,
                Err(_) => { ok = false; break; }
            }
        }
        if ok {
            return Some(ip);
        }
    }
    edos_lib::net::dns_resolve(s)
}
