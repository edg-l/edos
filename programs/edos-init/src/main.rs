//! The first userspace process: starts the session and keeps it running.
//!
//! The kernel spawns exactly this, and everything about which programs make up
//! a session lives here rather than in the kernel. Each service gets a
//! supervisor thread that spawns it, waits for it, and restarts it.

use std::thread;
use std::time::{Duration, Instant};

use edos_lib::process;

struct Service {
    /// Binary to run.
    path: &'static str,
    /// Whether the session is usable without it. A non-essential service that
    /// exhausts its restarts is left dead; an essential one is a louder problem
    /// but is still not worth rebooting the machine over.
    essential: bool,
}

const SERVICES: &[Service] = &[
    Service {
        path: "/bin/edos-wm",
        essential: true,
    },
    Service {
        path: "/bin/edos-taskbar",
        essential: false,
    },
    Service {
        path: "/bin/edos-terminal",
        essential: false,
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

fn supervise(service: &'static Service) {
    let name = service.path.rsplit('/').next().unwrap_or(service.path);
    let mut failures: u32 = 0;

    loop {
        let started = Instant::now();
        let pid = process::spawn(service.path, &[], 0, 1, 2);
        if pid == u64::MAX {
            failures += 1;
            eprintln!("init: {name}: spawn failed (attempt {failures})");
        } else {
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
