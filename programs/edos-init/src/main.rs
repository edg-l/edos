//! The first userspace process: starts the session and keeps it running.
//!
//! The kernel spawns exactly this, and everything about which programs make up
//! a session lives here rather than in the kernel. Each service gets a
//! supervisor thread that spawns it, waits for it, and restarts it.
//!
//! Which services exist comes from two places: the desktop session is compiled
//! in, so a filesystem with nothing on it still boots to a usable machine, and
//! `/etc/services/*.conf` adds to it. Runtime control arrives on a FIFO and is
//! reported back through a status file; see [`control`].

use std::fs::File;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use edos_lib::process;
use edos_lib::process::grant_shell;

mod control;
mod service;

use control::{Control, RunState};
use service::{Restart, Service};

/// A service that dies faster than this is treated as failing to start rather
/// than as having run and exited.
const HEALTHY_RUNTIME: Duration = Duration::from_secs(10);

/// Restart backoff, indexed by consecutive failures, then capped.
const BACKOFF_MS: &[u64] = &[100, 250, 500, 1000, 2000, 5000];

/// Consecutive rapid failures before a service is given up on. Without this a
/// binary that crashes on startup pins a CPU respawning forever.
const MAX_RAPID_FAILURES: u32 = 5;

/// Offset from UTC the session starts with, as an ISO 8601 offset. Change it
/// with `export TZ=<offset>` in a shell, which every program that shell starts
/// then picks up.
const DEFAULT_TZ: &str = "+02:00";

/// The session's home directory, and the user it belongs to. There is no user
/// database yet, so this single account is the whole of it. Every service
/// inherits both, and the working directory below, which is how a terminal
/// opens somewhere other than `/`.
const HOME: &str = "/home/edos";
const USER: &str = "edos";

/// How long to wait for a service's devices before giving up and spawning it
/// anyway. Sized to clear the longest driver initialization on the boot path:
/// the network driver waits up to 5 s for a DHCP lease, and an input driver
/// whose kthread lands behind it registers only once that wait is over.
const DEVICE_WAIT: Duration = Duration::from_secs(15);

/// Poll interval while waiting for a device node to appear. There is no
/// notification for "a device was registered", so this is a poll.
const DEVICE_POLL: Duration = Duration::from_millis(50);

/// Wait until every path opens. Returns the ones that never appeared, so the
/// caller can decide whether to start the service without them.
fn wait_for_devices(paths: &[String]) -> Vec<String> {
    let mut missing: Vec<String> = paths
        .iter()
        .filter(|p| File::open(p).is_err())
        .cloned()
        .collect();
    if missing.is_empty() {
        return missing;
    }

    println!("init: waiting for {missing:?}");
    let deadline = Instant::now() + DEVICE_WAIT;
    while Instant::now() < deadline {
        thread::sleep(DEVICE_POLL);
        missing.retain(|p| File::open(p).is_err());
        if missing.is_empty() {
            break;
        }
    }
    missing
}

fn supervise(service: Arc<Service>, control: Arc<Control>) {
    let name = service.name.as_str();

    if !service.enabled() {
        let config = service.enabled_by.as_deref().unwrap_or("");
        println!("init: {name} not started: no {config}");
        // Failed rather than stopped: it is down because it is not configured,
        // which `svc start` cannot fix and should not silently paper over.
        control.update(name, |e| {
            e.want_up = false;
            e.state = RunState::Failed;
        });
        return;
    }

    // Once, before the first spawn: a restart later on cannot lose this race,
    // since the drivers registered long before.
    let missing = wait_for_devices(&service.requires);
    if !missing.is_empty() {
        eprintln!(
            "init: {name}: {missing:?} did not appear within {DEVICE_WAIT:?}; starting it anyway"
        );
    }

    let args: Vec<&str> = service.args.iter().map(String::as_str).collect();

    loop {
        // Park until the service is wanted up. A stopped service costs nothing
        // here: nothing wakes this thread until `svc start` does.
        control.wait_until(name, |e| e.want_up);

        let started = Instant::now();
        let pid = process::spawn_with_env(&service.command, &args, 0, 1, 2);
        if let Ok(pid) = pid {
            if service.shell
                && let Err(e) = grant_shell(pid)
            {
                eprintln!("init: {name}: could not grant shell privilege: {e:?}");
            }
            control.update(name, |e| {
                e.pid = pid;
                e.state = RunState::Running;
            });
            println!("init: {name} started, pid {pid}");

            let code = process::waitpid(pid);
            let ran_for = started.elapsed();
            let wanted_up = control
                .update(name, |e| {
                    e.pid = 0;
                    e.want_up
                })
                .unwrap_or(true);

            if !wanted_up {
                // It exited because it was told to. Not a failure, and not
                // something to back off from.
                println!("init: {name} stopped");
                control.update(name, |e| {
                    e.state = RunState::Stopped;
                    e.failures = 0;
                });
                continue;
            }

            // A service that finished rather than failed is taken at its word,
            // where its policy says to. `code` was previously only ever
            // printed, so a window the user closed and a process that died
            // were indistinguishable and both came back.
            let policy = service.restart;
            let finished = code == 0;
            if policy == Restart::Never || (policy == Restart::OnFailure && finished) {
                println!("init: {name} exited with {code}, not restarting");
                control.update(name, |e| {
                    e.state = RunState::Stopped;
                    e.failures = 0;
                    e.want_up = false;
                });
                continue;
            }

            if ran_for >= HEALTHY_RUNTIME {
                // It came up, did its job, and exited. Restart it promptly:
                // this is the ordinary case for a crash after a long run.
                println!("init: {name} exited with {code} after {ran_for:?}, restarting");
                control.update(name, |e| {
                    e.failures = 0;
                    e.state = RunState::Backoff;
                });
                continue;
            }

            let failures = control
                .update(name, |e| {
                    e.failures += 1;
                    e.state = RunState::Backoff;
                    e.failures
                })
                .unwrap_or(0);
            eprintln!(
                "init: {name} exited with {code} after only {ran_for:?} (failure {failures})"
            );
        } else {
            control.update(name, |e| {
                e.failures += 1;
                e.pid = 0;
                e.state = RunState::Backoff;
            });
            let failures = control.with(name, |e| e.failures).unwrap_or(0);
            eprintln!("init: {name}: spawn failed (attempt {failures})");
        }

        let failures = control.with(name, |e| e.failures).unwrap_or(0);
        if failures >= MAX_RAPID_FAILURES {
            let kind = if service.essential {
                "essential service"
            } else {
                "service"
            };
            eprintln!(
                "init: giving up on {kind} {name} after {failures} rapid failures; \
                 start it again with `svc start {name}`"
            );
            // Down, but reachable: `svc start` clears the failure count and
            // wakes the wait at the top of the loop, which is what makes this
            // recoverable without a reboot.
            control.update(name, |e| {
                e.want_up = false;
                e.state = RunState::Failed;
            });
            continue;
        }

        // Backoff, cut short if someone asks for a start in the meantime: a
        // restart command during a five-second wait should not have to serve
        // out the wait it is overriding.
        let idx = (failures.max(1) as usize - 1).min(BACKOFF_MS.len() - 1);
        let generation = control.with(name, |e| e.generation).unwrap_or(0);
        let deadline = Instant::now() + Duration::from_millis(BACKOFF_MS[idx]);
        while Instant::now() < deadline {
            let moved = control
                .with(name, |e| e.generation != generation || !e.want_up)
                .unwrap_or(true);
            if moved {
                break;
            }
            thread::sleep(DEVICE_POLL);
        }
    }
}

fn main() {
    let services = service::load();
    println!(
        "init: pid {}, starting {} services",
        process::getpid(),
        services.len()
    );

    // The session's clock offset from UTC. The kernel keeps time in UTC and
    // there is no zone database, so this fixed ISO 8601 offset is the whole of
    // the system's timezone support; it is inherited by every service and so by
    // the panel clock and anything the shell runs.
    unsafe {
        std::env::set_var("TZ", DEFAULT_TZ);
        std::env::set_var("HOME", HOME);
        std::env::set_var("USER", USER);
    }

    // The working directory is inherited across spawn, so setting it here is
    // what puts the session's shell in the home directory. A root that has no
    // `/home/edos` -- an older installed disk -- keeps the one it booted with.
    if let Err(e) = std::env::set_current_dir(HOME) {
        eprintln!("init: {}: {}, staying in /", HOME, e);
    }

    let names: Vec<String> = services.iter().map(|s| s.name.clone()).collect();
    let control = Control::new(&names);
    control::prepare();
    control.publish_now();

    // One supervisor thread per service, because waitpid names a single child
    // and each service restarts on its own schedule.
    let mut supervisors = Vec::new();
    for service in services {
        let control = control.clone();
        supervisors.push(thread::spawn(move || supervise(service, control)));
    }

    // The control channel outlives every service: a system whose services have
    // all failed is exactly when being able to start one matters.
    let controller = {
        let control = control.clone();
        thread::spawn(move || control::serve(control))
    };

    for handle in supervisors {
        let _ = handle.join();
    }
    let _ = controller.join();

    // Every service has given up. Stay alive anyway: init is where orphans are
    // reparented, and exiting would leave them with no collector at all.
    eprintln!("init: no services left running; idling as the orphan reaper");
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
