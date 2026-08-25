//! Checks that a kill reaches a process whatever it is doing.
//!
//! Two modes matter and they take different kernel paths. A process blocked in
//! a syscall observes `killed` when that syscall returns; a process spinning in
//! user code makes no syscalls at all and can only be caught by a timer tick
//! that interrupted ring 3.
//!
//! The driver spawns itself in one of those modes, signals it, and waits with a
//! bound. A miss is a process that never dies, so the wait has to be polled
//! rather than blocking in `waitpid`, or the failure would show up as this
//! program hanging instead of reporting.

use std::env;
use std::hint::black_box;
use std::thread::sleep;
use std::time::Duration;

use edos_lib::process::{self, waitpid_nonblocking};

const SELF_PATH: &str = "/bin/killtest";
const SIGINT: u32 = 2;
/// The kernel reports a signalled death as 128 + signal.
const SIGNALLED: i32 = 128 + SIGINT as i32;

/// Long enough for many timer ticks (the timeslice is 5ms), short enough that a
/// failure is reported rather than waited out.
const POLL_ATTEMPTS: u32 = 100;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

fn fail(test: &str, msg: &str) -> ! {
    eprintln!("FAIL {}: {}", test, msg);
    std::process::exit(1);
}

fn pass(test: &str, detail: &str) {
    println!("PASS {}: {}", test, detail);
}

/// Spawn a child in `mode` and return its pid once it reports being in position.
///
/// The handshake is what makes the test mean something: killing a child that is
/// still in its runtime's startup would exercise the syscall boundary no matter
/// which mode was asked for.
fn spawn_ready(test: &str, mode: &str) -> u64 {
    let Some((read_fd, write_fd)) = process::pipe() else {
        fail(test, "pipe failed");
    };
    let child = process::spawn(SELF_PATH, &[mode], 0, write_fd, 2);
    process::close(write_fd);
    let Ok(child) = child else {
        fail(test, "spawn failed");
    };

    let mut buf = [0u8; 8];
    let n = process::read(read_fd, &mut buf);
    process::close(read_fd);
    if !matches!(n, Ok(n) if n > 0) {
        fail(test, "child never reported ready");
    }
    // The child wrote from inside a syscall; give it time to return to user
    // code so the kill lands where this mode intends it to.
    sleep(Duration::from_millis(20));
    child
}

/// Signal `child` and collect its status, or report how long it survived.
fn kill_and_reap(test: &str, child: u64) -> i32 {
    if process::kill(child, SIGINT).is_err() {
        fail(test, "kill reported failure");
    }
    for _ in 0..POLL_ATTEMPTS {
        if let Some(code) = waitpid_nonblocking(child) {
            return code;
        }
        sleep(POLL_INTERVAL);
    }
    fail(
        test,
        &format!(
            "child {} was still alive {}ms after the signal",
            child,
            POLL_ATTEMPTS * POLL_INTERVAL.as_millis() as u32
        ),
    );
}

fn check(test: &str, mode: &str, detail: &str) {
    let child = spawn_ready(test, mode);
    let code = kill_and_reap(test, child);
    if code != SIGNALLED {
        fail(test, &format!("expected exit {}, got {}", SIGNALLED, code));
    }
    pass(test, detail);
}

fn ready() {
    let _ = process::write(1, b"ready\n");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        // No syscalls past the handshake, so nothing here can observe the kill.
        Some("spin") => {
            ready();
            let mut n = 0u64;
            loop {
                n = black_box(n).wrapping_add(1);
            }
        }
        Some("sleeper") => {
            ready();
            sleep(Duration::from_secs(3600));
            std::process::exit(0);
        }
        _ => {}
    }

    check(
        "test 1",
        "spin",
        "a kill reached a thread spinning in user code",
    );
    check(
        "test 2",
        "sleeper",
        "a kill reached a thread blocked in a syscall",
    );

    println!("killtest: all tests passed");
    std::process::exit(0);
}
