use std::{env, fs};

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut show_all = false;
    let mut show_sys = false;
    let mut show_release = false;
    let mut show_machine = false;

    for arg in &args[1..] {
        if let Some(flags) = arg.strip_prefix('-') {
            for c in flags.chars() {
                match c {
                    'a' => show_all = true,
                    's' => show_sys = true,
                    'r' => show_release = true,
                    'm' => show_machine = true,
                    _ => {
                        eprintln!("uname: unknown option -{}", c);
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    if !show_sys && !show_release && !show_machine && !show_all {
        show_sys = true;
    }

    // `/proc/version` is "EDOS <release> <machine>", rendered from the kernel's
    // own `CARGO_PKG_VERSION`. Reading it keeps the release out of userspace: a
    // copy here reported 0.1.0 for two releases without anything noticing.
    let version = fs::read_to_string("/proc/version").unwrap_or_default();
    let mut fields = version.split_whitespace();
    let sysname = fields.next().unwrap_or("EDOS");
    let release = fields.next().unwrap_or("unknown");
    let machine = fields.next().unwrap_or("x86_64");

    let mut parts: Vec<&str> = Vec::new();
    if show_all || show_sys {
        parts.push(sysname);
    }
    if show_all || show_release {
        parts.push(release);
    }
    if show_all || show_machine {
        parts.push(machine);
    }

    println!("{}", parts.join(" "));
}
