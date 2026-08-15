//! Regression suite for `lookupd`, the caching name lookup daemon.
//!
//! The wire format and the cache have host unit tests in
//! `programs/lookupd/src/dns.rs`; those need no guest. What needs a guest is
//! everything this file covers: that the kernel's resolver override is in the
//! path of an ordinary program, that a second PROCESS gets the benefit of the
//! first one's lookup, and that an override whose owner is gone stops being
//! used rather than swallowing every query.
//!
//! Reports through its exit code, so `make guest-check` can judge it.
//!
//! Needs a working upstream resolver: the cases below turn on an answer coming
//! back at all. `socktest` has the same dependency.

use std::net::ToSocketAddrs;
use std::process::exit;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use edos_lib::{
    net,
    process::{self, SIGHUP},
};

/// Where `lookupd` publishes its counters.
const STATS: &str = "/tmp/lookupd.stats";

const LOOPBACK: [u8; 4] = [127, 0, 0, 1];

/// An address with nothing listening on it, used to prove that an override
/// belonging to a dead process stops being honoured.
const NOWHERE: [u8; 4] = [127, 0, 0, 2];

/// A name that resolves, looked up repeatedly so the second answer is a hit.
const NAME: &str = "example.com";

/// How long a counter is waited on. `lookupd` publishes at most once a second,
/// so a test that reads immediately after a lookup reads the value from before
/// it: the counter is the observable, and it arrives on the daemon's schedule.
const STATS_BUDGET: Duration = Duration::from_secs(3);

/// A lookup must not take longer than this. Three attempts at the client's two
/// second timeout is six seconds, so anything at or above that is the shape of
/// a query going somewhere nothing answers.
const LOOKUP_BUDGET: Duration = Duration::from_secs(3);

static FAILURES: AtomicU32 = AtomicU32::new(0);

fn check(name: &str, ok: bool, detail: String) {
    if ok {
        println!("PASS {name}: {detail}");
    } else {
        println!("FAIL {name}: {detail}");
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

/// One counter out of `/tmp/lookupd.stats`, which is `key: value` per line.
fn stat(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string(STATS).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|v| v.trim().parse().ok())
}

/// A line out of `/proc/net`.
fn proc_net(key: &str) -> Option<String> {
    let text = std::fs::read_to_string("/proc/net").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
        .map(|v| v.trim().to_string())
}

/// Resolve `NAME`, returning how long it took, or None if it did not resolve.
fn lookup() -> Option<Duration> {
    let started = Instant::now();
    let ok = (NAME, 0u16)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some();
    ok.then(|| started.elapsed())
}

/// `lookupd`'s pid, read out of `/proc/processes`.
///
/// The columns are PID PPID PGID TYPE STATE PRIO CPU CPUms RSSKiB NAME, and
/// NAME is the command line, so it is matched on its basename: init spawns the
/// service by path.
fn lookupd_pid() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/processes").ok()?;
    text.lines()
        .find(|line| {
            line.split_whitespace()
                .nth(9)
                .and_then(|cmd| cmd.rsplit('/').next())
                == Some("lookupd")
        })
        .and_then(|line| line.split_whitespace().next()?.parse().ok())
}

/// Wait for `f` to hold, up to `budget`, so a test never turns on a fixed sleep.
fn wait_for(budget: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    f()
}

/// The child half of case 5: install an override and exit at once, leaving the
/// kernel holding one whose owner is gone.
fn steal_and_exit() -> ! {
    let code = i32::from(net::set_dns(NOWHERE).is_err());
    exit(code);
}

/// The daemon is running and the kernel is pointing lookups at it.
fn case_override_is_installed() {
    let resolver = proc_net("resolver");
    check(
        "override",
        resolver.as_deref() == Some("127.0.0.1"),
        format!("/proc/net resolver: {resolver:?}"),
    );

    let dns = net::get_dns();
    check(
        "getdns",
        dns == Some(LOOPBACK),
        format!("getdns returned {dns:?}"),
    );
}

/// A name looked up twice is answered from the cache the second time.
fn case_repeat_is_a_hit() {
    let before = stat("hits");
    if lookup().is_none() {
        check(
            "repeat-hit",
            false,
            format!("{NAME} did not resolve at all; is the upstream reachable?"),
        );
        return;
    }
    let took = lookup();
    let counted = wait_for(STATS_BUDGET, || stat("hits") > before);

    check(
        "repeat-hit",
        took.is_some() && counted,
        format!(
            "hits {before:?} -> {:?}, second lookup took {took:?}",
            stat("hits")
        ),
    );
}

/// A SECOND PROCESS is answered from the first one's lookup.
///
/// This is the case that separates a system-wide cache from one inside a
/// library: nothing in this process is reused, and the answer still comes back
/// without a query on the wire.
fn case_another_process_gets_the_hit() {
    let before = stat("hits");
    let pid = process::spawn("/bin/dns", &[NAME], 0, 1, 2);
    if pid == u64::MAX {
        check(
            "cross-process-hit",
            false,
            "could not spawn /bin/dns".into(),
        );
        return;
    }
    let code = process::waitpid(pid);
    let counted = wait_for(STATS_BUDGET, || stat("hits") > before);

    check(
        "cross-process-hit",
        code == 0 && counted,
        format!("dns exited {code}, hits {before:?} -> {:?}", stat("hits")),
    );
}

/// `SIGHUP` empties the cache without resetting what it has done.
///
/// The counters are asserted not to go BACKWARDS rather than to be unchanged:
/// anything else on the machine may resolve a name while this runs, and a
/// flush that reset the totals would still be caught.
fn case_hup_flushes() {
    let Some(pid) = lookupd_pid() else {
        check("flush", false, "lookupd is not in /proc/processes".into());
        return;
    };
    let hits_before = stat("hits");

    process::kill(pid, SIGHUP);
    let emptied = wait_for(Duration::from_secs(3), || stat("entries") == Some(0));
    let hits_after = stat("hits");

    check(
        "flush",
        emptied && hits_after >= hits_before,
        format!(
            "entries {:?}, hits {hits_before:?} -> {hits_after:?}",
            stat("entries")
        ),
    );
}

/// An override whose owner exited is not used, and the lookup that follows it
/// answers from the DHCP address instead of being swallowed.
///
/// Without the revocation this is the six second stall described in
/// `doc/design/lookupd.md`: nothing listens on `NOWHERE`, and this kernel sends
/// no ICMP port unreachable, so the client would wait out every attempt.
fn case_a_dead_owner_does_not_hold_the_override() {
    let pid = process::spawn("/bin/lookuptest", &["--steal"], 0, 1, 2);
    if pid == u64::MAX {
        check(
            "revocation",
            false,
            "could not spawn the stealing child".into(),
        );
        return;
    }
    if process::waitpid(pid) != 0 {
        check(
            "revocation",
            false,
            "the child could not set an override".into(),
        );
        return;
    }

    let dns = net::get_dns();
    check(
        "revocation",
        dns != Some(NOWHERE),
        format!("after the owner exited, getdns returned {dns:?}"),
    );

    match lookup() {
        Some(took) => check(
            "fallback",
            took < LOOKUP_BUDGET,
            format!("{NAME} resolved in {took:?}"),
        ),
        None => check(
            "fallback",
            false,
            format!("{NAME} did not resolve once the override was revoked"),
        ),
    }
}

/// The daemon takes the override back after something else displaced it.
fn case_the_daemon_reclaims_the_override() {
    let reclaimed = wait_for(Duration::from_secs(5), || net::get_dns() == Some(LOOPBACK));
    check(
        "reclaim",
        reclaimed,
        format!("getdns is {:?}", net::get_dns()),
    );
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--steal") {
        steal_and_exit();
    }

    if lookupd_pid().is_none() {
        println!("FAIL lookupd: not running; /etc/lookupd.conf must exist at boot");
        exit(1);
    }

    case_override_is_installed();
    case_repeat_is_a_hit();
    case_another_process_gets_the_hit();
    case_hup_flushes();
    case_a_dead_owner_does_not_hold_the_override();
    case_the_daemon_reclaims_the_override();

    let failures = FAILURES.load(Ordering::Relaxed);
    if failures == 0 {
        println!("lookuptest: all cases passed");
        exit(0);
    }
    println!("lookuptest: {failures} case(s) failed");
    exit(1);
}
