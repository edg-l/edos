//! svc - start, stop and inspect the services init supervises.
//!
//! Commands go to init on a FIFO and answers come back from the status file
//! init rewrites; this program does no supervising of its own. That division
//! is deliberate and is the reason a control program can be this small: the
//! process holding `waitpid` and the restart backoff is the only one that can
//! act on a service, so everything here is either a line written to a pipe or a
//! read of a file.

use std::fs;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use edos_lib::io;

/// Where init listens. Mirrors `edos-init`'s `control::CONTROL_FIFO`.
const CONTROL_FIFO: &str = "/var/run/svc.ctl";

/// Where init publishes. Mirrors `edos-init`'s `control::STATUS_FILE`.
const STATUS_FILE: &str = "/var/run/svc.status";

/// How long to wait for a command to show up in the status file before
/// reporting what the service is actually doing. Init acts on a command as soon
/// as it reads it, so this only has to cover a service's own exit.
const SETTLE: Duration = Duration::from_secs(5);

const SETTLE_POLL: Duration = Duration::from_millis(50);

struct Status {
    name: String,
    state: String,
    pid: u64,
    failures: u32,
}

fn read_status() -> Result<Vec<Status>, String> {
    let text = fs::read_to_string(STATUS_FILE)
        .map_err(|e| format!("{STATUS_FILE}: {e}; is init running?"))?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some(Status {
                name: parts.next()?.to_string(),
                state: parts.next()?.to_string(),
                pid: parts.next()?.parse().ok()?,
                failures: parts.next()?.parse().ok()?,
            })
        })
        .collect())
}

fn send(command: &str, name: &str) -> Result<(), String> {
    // O_NONBLOCK, so that a control FIFO left behind by an init that is no
    // longer running fails here instead of waiting forever for a reader that
    // is never going to arrive. That is the case POSIX gives ENXIO for.
    let fd = io::open(CONTROL_FIFO, io::O_WRONLY | io::O_NONBLOCK);
    if fd < 0 {
        return Err(format!(
            "{CONTROL_FIFO}: {:?}; is init running?",
            io::last_errno()
        ));
    }
    let fd = fd as u64;

    // The flag governs the write too, so a control FIFO that init has stopped
    // draining reports EAGAIN rather than parking here. A short write is the
    // same case seen from the other side and is worth saying so: half a command
    // line reaches init as a command it cannot parse.
    let line = format!("{command} {name}\n");
    let written = io::sys_write(fd, line.as_bytes());
    io::close(fd);
    if written < 0 {
        return Err(format!("{CONTROL_FIFO}: {:?}", io::last_errno()));
    }
    if written as usize != line.len() {
        return Err(format!(
            "{CONTROL_FIFO}: wrote {written} of {} bytes; is init reading?",
            line.len()
        ));
    }
    Ok(())
}

fn state_of(name: &str) -> Option<String> {
    read_status()
        .ok()?
        .into_iter()
        .find(|s| s.name == name)
        .map(|s| s.state)
}

/// Wait until `name` reaches one of `wanted`, and report where it ended up.
fn settle(name: &str, wanted: &[&str]) -> Option<String> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let state = state_of(name)?;
        if wanted.contains(&state.as_str()) || Instant::now() >= deadline {
            return Some(state);
        }
        sleep(SETTLE_POLL);
    }
}

fn known(name: &str) -> Result<bool, String> {
    Ok(read_status()?.iter().any(|s| s.name == name))
}

fn list() -> Result<(), String> {
    let statuses = read_status()?;
    let width = statuses
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0)
        .max(7);
    println!(
        "{:<width$}  {:<8}  {:>6}  {}",
        "SERVICE", "STATE", "PID", "FAILURES"
    );
    for s in statuses {
        let pid = if s.pid == 0 {
            "-".to_string()
        } else {
            s.pid.to_string()
        };
        println!(
            "{:<width$}  {:<8}  {:>6}  {}",
            s.name, s.state, pid, s.failures
        );
    }
    Ok(())
}

fn status(name: &str) -> Result<bool, String> {
    let statuses = read_status()?;
    let Some(s) = statuses.iter().find(|s| s.name == name) else {
        return Err(format!("no service named {name:?}"));
    };
    if s.pid == 0 {
        println!("{}: {} ({} failures)", s.name, s.state, s.failures);
    } else {
        println!(
            "{}: {}, pid {} ({} failures)",
            s.name, s.state, s.pid, s.failures
        );
    }
    Ok(s.state == "running")
}

fn act(command: &str, name: &str) -> Result<bool, String> {
    if !known(name)? {
        return Err(format!("no service named {name:?}"));
    }
    send(command, name)?;

    let wanted: &[&str] = match command {
        "stop" => &["stopped", "failed"],
        // A restart passes through `backoff` on its way back up, so waiting for
        // `running` is what distinguishes a restart that worked from one whose
        // service died again immediately.
        _ => &["running"],
    };
    let Some(state) = settle(name, wanted) else {
        return Err(format!("{name}: init stopped reporting its state"));
    };
    println!("{name}: {state}");
    Ok(wanted.contains(&state.as_str()))
}

fn usage() {
    eprintln!("Usage: svc <command> [service]");
    eprintln!();
    eprintln!("  list                 what every service is doing");
    eprintln!("  status <service>     one service, in full");
    eprintln!("  start <service>      start it, and clear its failure count");
    eprintln!("  stop <service>       stop it, and do not restart it");
    eprintln!("  restart <service>    stop it if it is up, then start it");
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        usage();
        return ExitCode::FAILURE;
    };

    let result = match (command, args.get(1)) {
        ("list", None) => list().map(|()| true),
        ("status", Some(name)) => status(name),
        ("start" | "stop" | "restart", Some(name)) => act(command, name),
        ("-h" | "--help" | "help", _) => {
            usage();
            return ExitCode::SUCCESS;
        }
        (_, None) => Err(format!("{command} needs a service name")),
        _ => Err(format!("unknown command {command:?}")),
    };

    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("svc: {e}");
            ExitCode::FAILURE
        }
    }
}
