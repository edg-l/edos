use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut show_all = false;
    let mut show_sys = false;
    let mut show_release = false;
    let mut show_machine = false;

    for arg in &args[1..] {
        if arg.starts_with('-') {
            for c in arg[1..].chars() {
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

    let mut parts: Vec<&str> = Vec::new();
    if show_all || show_sys {
        parts.push("EDOS");
    }
    if show_all || show_release {
        parts.push("0.1.0");
    }
    if show_all || show_machine {
        parts.push("x86_64");
    }

    println!("{}", parts.join(" "));
}
