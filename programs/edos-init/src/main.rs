//! The first userspace process: starts the session and keeps it running.
//!
//! The kernel spawns exactly this, and everything about which programs make up
//! a session lives here rather than in the kernel. Each service gets a
//! supervisor thread that spawns it, waits for it, and restarts it.

use std::fs::File;
use std::thread;
use std::time::{Duration, Instant};

use edos_lib::process;
use edos_lib::process::grant_shell;

struct Service {
    /// Binary to run.
    path: &'static str,
    /// Whether the session is usable without it. A non-essential service that
    /// exhausts its restarts is left dead; an essential one is a louder problem
    /// but is still not worth rebooting the machine over.
    essential: bool,
    /// Whether this service manages other processes' windows, and so needs the
    /// privilege to move, resize, frame, minimize and focus them.
    ///
    /// Granted per spawn, since the privilege is per pid and dies with the
    /// process. Init holds it because the kernel starts init and nothing else,
    /// which makes "what a session is" init's decision rather than a race
    /// between whichever program claims it first.
    shell: bool,
    /// Device nodes that must exist before the service is worth spawning.
    ///
    /// Drivers register their `/dev` entries from kthreads, so a node appears
    /// some time after userspace starts rather than before it. A service that
    /// opens one during startup otherwise races the driver and dies, and a
    /// service that treats the open as optional comes up permanently without
    /// that device. Waiting here keeps both out of every service.
    requires: &'static [&'static str],
}

const SERVICES: &[Service] = &[
    Service {
        path: "/bin/edos-wm",
        essential: true,
        shell: true,
        requires: &["/dev/mouse", "/dev/kbd"],
    },
    Service {
        path: "/bin/edos-taskbar",
        essential: false,
        shell: true,
        requires: &[],
    },
    Service {
        path: "/bin/edos-terminal",
        essential: false,
        shell: false,
        requires: &[],
    },
];

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
fn wait_for_devices(paths: &'static [&'static str]) -> Vec<&'static str> {
    let mut missing: Vec<&'static str> = paths
        .iter()
        .copied()
        .filter(|p| File::open(p).is_err())
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

fn supervise(service: &'static Service) {
    let name = service.path.rsplit('/').next().unwrap_or(service.path);
    let mut failures: u32 = 0;

    // Once, before the first spawn: a restart later on cannot lose this race,
    // since the drivers registered long before.
    let missing = wait_for_devices(service.requires);
    if !missing.is_empty() {
        eprintln!(
            "init: {name}: {missing:?} did not appear within {DEVICE_WAIT:?}; starting it anyway"
        );
    }

    loop {
        let started = Instant::now();
        let pid = process::spawn_with_env(service.path, &[], 0, 1, 2);
        if pid == u64::MAX {
            failures += 1;
            eprintln!("init: {name}: spawn failed (attempt {failures})");
        } else {
            if service.shell {
                if let Err(e) = grant_shell(pid) {
                    eprintln!("init: {name}: could not grant shell privilege: {e}");
                }
            }
            println!("init: {name} started, pid {pid}");
            let code = process::waitpid(pid);
            let ran_for = started.elapsed();

            if ran_for >= HEALTHY_RUNTIME {
                // It came up, did its job, and exited. Restart it promptly:
                // this is the ordinary case for a terminal the user closed.
                println!("init: {name} exited with {code} after {ran_for:?}, restarting");
                failures = 0;
                continue;
            }

            failures += 1;
            eprintln!(
                "init: {name} exited with {code} after only {ran_for:?} (failure {failures})"
            );
        }

        if failures >= MAX_RAPID_FAILURES {
            let kind = if service.essential {
                "essential service"
            } else {
                "service"
            };
            eprintln!(
                "init: giving up on {kind} {name} after {failures} rapid failures; \
                 fix it and restart it by hand"
            );
            return;
        }

        let idx = (failures as usize - 1).min(BACKOFF_MS.len() - 1);
        thread::sleep(Duration::from_millis(BACKOFF_MS[idx]));
    }
}

fn main() {
    println!(
        "init: pid {}, starting {} services",
        process::getpid(),
        SERVICES.len()
    );

    // The session's clock offset from UTC. The kernel keeps time in UTC and
    // there is no zone database, so this fixed ISO 8601 offset is the whole of
    // the system's timezone support; it is inherited by every service and so by
    // the panel clock and anything the shell runs.
    unsafe { std::env::set_var("TZ", DEFAULT_TZ) };

    // One supervisor thread per service, because waitpid names a single child
    // and each service restarts on its own schedule.
    let mut supervisors = Vec::new();
    for service in SERVICES {
        supervisors.push(thread::spawn(move || supervise(service)));
    }

    for handle in supervisors {
        let _ = handle.join();
    }

    // Every service has given up. Stay alive anyway: init is where orphans are
    // reparented, and exiting would leave them with no collector at all.
    eprintln!("init: no services left running; idling as the orphan reaper");
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
