//! netstat - sockets, interfaces and routes
//!
//! The socket list comes from `/proc/sockets`, which is the kernel's TCP
//! connection table plus the port-table bindings that have no connection of
//! their own. That is deliberately not the same set as `lsof` reports: a
//! connection outlives the descriptor that made it, so a `TIME_WAIT` or a
//! `FIN_WAIT2` appears here and nowhere else, and those are exactly the states
//! worth looking at when a port cannot be bound again.
//!
//! Interfaces and routes come from `/proc/net`, the same file the panel's
//! network indicator reads.

use std::fs;
use std::process::ExitCode;

/// One row of `/proc/sockets`.
struct Socket {
    proto: String,
    recv_q: u64,
    send_q: u64,
    local: String,
    foreign: String,
    state: String,
}

/// A socket that is waiting for someone to connect to it, rather than one that
/// is carrying or has carried traffic.
impl Socket {
    fn is_server(&self) -> bool {
        self.state == "LISTEN"
            || self.state == "BOUND"
            || (self.proto == "udp" && self.state == "-")
    }
}

struct Options {
    /// Servers as well as connections.
    all: bool,
    /// Servers only.
    listening: bool,
    /// Protocol filters. Both false means both protocols.
    tcp: bool,
    udp: bool,
    /// Interface table instead of sockets.
    interfaces: bool,
    /// Routing table instead of sockets.
    routes: bool,
}

fn usage() -> ! {
    eprintln!("usage: netstat [-a] [-l] [-t] [-u] [-i] [-r]");
    std::process::exit(2)
}

fn parse_args() -> Options {
    let mut options = Options {
        all: false,
        listening: false,
        tcp: false,
        udp: false,
        interfaces: false,
        routes: false,
    };

    for arg in std::env::args().skip(1) {
        if arg == "--help" {
            usage();
        }
        let Some(flags) = arg.strip_prefix('-') else {
            eprintln!("netstat: unexpected operand '{arg}'");
            usage();
        };
        if flags.is_empty() {
            usage();
        }
        for flag in flags.chars() {
            match flag {
                'a' => options.all = true,
                'l' => options.listening = true,
                't' => options.tcp = true,
                'u' => options.udp = true,
                'i' => options.interfaces = true,
                'r' => options.routes = true,
                other => {
                    eprintln!("netstat: unknown option '-{other}'");
                    usage();
                }
            }
        }
    }

    options
}

fn read_proc(path: &str) -> Result<String, ExitCode> {
    fs::read_to_string(path).map_err(|err| {
        eprintln!("netstat: {path}: {err}");
        ExitCode::from(1)
    })
}

/// `PROTO RECVQ SENDQ LOCAL FOREIGN STATE`, one socket per line after the
/// header. A malformed line is skipped rather than aborting the listing: the
/// file is generated, so a short line means a field this build does not know.
fn parse_sockets(text: &str) -> Vec<Socket> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() != 6 {
                return None;
            }
            Some(Socket {
                proto: fields[0].to_string(),
                recv_q: fields[1].parse().ok()?,
                send_q: fields[2].parse().ok()?,
                local: fields[3].to_string(),
                foreign: fields[4].to_string(),
                state: fields[5].to_string(),
            })
        })
        .collect()
}

fn show_sockets(options: &Options) -> Result<(), ExitCode> {
    let sockets = parse_sockets(&read_proc("/proc/sockets")?);
    let both = !options.tcp && !options.udp;

    let title = if options.listening {
        "Active internet connections (only servers)"
    } else if options.all {
        "Active internet connections (servers and established)"
    } else {
        "Active internet connections (w/o servers)"
    };
    println!("{title}");
    println!(
        "{:<5} {:>6} {:>6} {:<17} {:<17} {}",
        "Proto", "Recv-Q", "Send-Q", "Local Address", "Foreign Address", "State"
    );

    for socket in sockets {
        if !both
            && !((options.tcp && socket.proto == "tcp") || (options.udp && socket.proto == "udp"))
        {
            continue;
        }
        if options.listening && !socket.is_server() {
            continue;
        }
        if !options.listening && !options.all && socket.is_server() {
            continue;
        }
        println!(
            "{:<5} {:>6} {:>6} {:<17} {:<17} {}",
            socket.proto, socket.recv_q, socket.send_q, socket.local, socket.foreign, socket.state
        );
    }

    Ok(())
}

/// `/proc/net` is a run of `key: value` lines, one block per interface, blocks
/// separated by a blank line and each opened by `interface:`.
fn parse_interfaces(text: &str) -> Vec<Vec<(String, String)>> {
    let mut blocks: Vec<Vec<(String, String)>> = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim().to_string(), value.trim().to_string());
        if key == "interface" {
            blocks.push(Vec::new());
        }
        if let Some(block) = blocks.last_mut() {
            block.push((key, value));
        }
    }
    blocks
}

fn field<'a>(block: &'a [(String, String)], key: &str) -> &'a str {
    block
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
        .unwrap_or("-")
}

fn show_interfaces() -> Result<(), ExitCode> {
    let blocks = parse_interfaces(&read_proc("/proc/net")?);
    println!("Kernel Interface table");
    println!(
        "{:<8} {:<16} {:<6} {:<20} {}",
        "Iface", "Address", "Prefix", "HWaddr", "Flags"
    );
    for block in blocks {
        let up = field(&block, "link") == "up";
        println!(
            "{:<8} {:<16} {:<6} {:<20} {}",
            field(&block, "interface"),
            field(&block, "inet"),
            field(&block, "prefix"),
            field(&block, "mac"),
            if up { "U" } else { "-" }
        );
    }
    Ok(())
}

/// The kernel keeps no routing table: an address, a prefix and a gateway are
/// the whole of its forwarding decision, so the two routes it implies are
/// reconstructed here rather than invented in the kernel.
fn show_routes() -> Result<(), ExitCode> {
    let blocks = parse_interfaces(&read_proc("/proc/net")?);
    println!("Kernel IP routing table");
    println!(
        "{:<16} {:<16} {:<6} {}",
        "Destination", "Gateway", "Prefix", "Iface"
    );
    for block in blocks {
        let iface = field(&block, "interface");
        let inet = field(&block, "inet");
        let prefix = field(&block, "prefix");
        let gateway = field(&block, "gateway");
        if inet == "-" {
            continue;
        }
        if let Some(network) = network_of(inet, prefix) {
            println!("{network:<16} {:<16} {prefix:<6} {iface}", "0.0.0.0");
        }
        if gateway != "-" && gateway != "0.0.0.0" {
            println!("{:<16} {gateway:<16} {:<6} {iface}", "0.0.0.0", 0);
        }
    }
    Ok(())
}

/// The network part of `address`, i.e. the address with the host bits cleared.
fn network_of(address: &str, prefix: &str) -> Option<String> {
    let prefix: u32 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let mut octets = [0u8; 4];
    let mut parts = address.split('.');
    for octet in &mut octets {
        *octet = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let network = (u32::from_be_bytes(octets) & mask).to_be_bytes();
    Some(format!(
        "{}.{}.{}.{}",
        network[0], network[1], network[2], network[3]
    ))
}

fn main() -> ExitCode {
    let options = parse_args();

    let result = if options.interfaces {
        show_interfaces()
    } else if options.routes {
        show_routes()
    } else {
        show_sockets(&options)
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}
