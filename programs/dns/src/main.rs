use std::env;
use std::net::ToSocketAddrs;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: dns <hostname>");
        std::process::exit(2);
    }

    let hostname = &args[1];
    // Port 0: the resolver is what is being asked about, not a service.
    match (hostname.as_str(), 0u16).to_socket_addrs() {
        Ok(addrs) => {
            let mut found = false;
            for addr in addrs {
                println!("{} -> {}", hostname, addr.ip());
                found = true;
            }
            if !found {
                eprintln!("dns: {}: no addresses", hostname);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("dns: {}: {}", hostname, e);
            std::process::exit(1);
        }
    }
}
