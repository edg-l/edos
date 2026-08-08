use std::env;

use edos_lib::net;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: dns <hostname>");
        std::process::exit(2);
    }

    let hostname = &args[1];
    match net::dns_lookup(hostname) {
        Ok(ip) => println!("{} -> {}.{}.{}.{}", hostname, ip[0], ip[1], ip[2], ip[3]),
        Err(e) => {
            eprintln!("dns: {}: {}", hostname, e);
            std::process::exit(1);
        }
    }
}
