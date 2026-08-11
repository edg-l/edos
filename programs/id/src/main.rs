//! Print the calling process's user and group ids.
//!
//! There is no supplementary-group list and no way to become anything but the
//! id the kernel hands out, so effective and real ids are the same value and
//! `groups=` carries the one group. Names come from the fixed table in
//! `edos_lib::process::id_name`; an id with no name prints bare, without the
//! `(name)` suffix.

use std::process::ExitCode;

use edos_lib::process;

const USAGE: &str = "usage: id [-u|-g|-G] [-n]";

/// `0(root)`, or `1000` when the id has no name.
fn decorate(id: u32) -> String {
    match process::id_name(id) {
        Some(name) => format!("{id}({name})"),
        None => format!("{id}"),
    }
}

/// `root`, or `1000` when the id has no name -- what `-n` prints.
fn name_of(id: u32) -> String {
    process::id_name(id)
        .map(str::to_string)
        .unwrap_or_else(|| id.to_string())
}

fn main() -> ExitCode {
    let mut single: Option<char> = None;
    let mut names_only = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--name" => {
                names_only = true;
                continue;
            }
            "--real" => continue, // no setuid, so the real id is the only id
            "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
        // `id -un` is the usual spelling, so short options cluster.
        let Some(flags) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) else {
            eprintln!("id: unknown operand '{arg}'\n{USAGE}");
            return ExitCode::from(2);
        };
        for flag in flags.chars() {
            match flag {
                'u' | 'g' | 'G' => {
                    if single.is_some_and(|had| had != flag) {
                        eprintln!("id: cannot print more than one of -u, -g and -G\n{USAGE}");
                        return ExitCode::from(2);
                    }
                    single = Some(flag);
                }
                'n' => names_only = true,
                'r' => {} // no setuid, so the real id is the only id
                'h' => {
                    println!("{USAGE}");
                    return ExitCode::SUCCESS;
                }
                other => {
                    eprintln!("id: unknown option '-{other}'\n{USAGE}");
                    return ExitCode::from(2);
                }
            }
        }
    }

    let uid = process::getuid();
    let gid = process::getgid();

    match single {
        Some('u') => println!(
            "{}",
            if names_only {
                name_of(uid)
            } else {
                uid.to_string()
            }
        ),
        Some('g') | Some('G') => {
            println!(
                "{}",
                if names_only {
                    name_of(gid)
                } else {
                    gid.to_string()
                }
            )
        }
        Some(_) => unreachable!(),
        None => {
            if names_only {
                eprintln!("id: -n needs one of -u, -g or -G\n{USAGE}");
                return ExitCode::from(2);
            }
            println!(
                "uid={} gid={} groups={}",
                decorate(uid),
                decorate(gid),
                decorate(gid)
            );
        }
    }

    ExitCode::SUCCESS
}
