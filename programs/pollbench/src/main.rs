//! Measures what a `poll` call costs, split into the part that is fixed per
//! call and the part that scales with the descriptor count.
//!
//! Every shape polls with a zero timeout, so no case ever sleeps:
//!
//! - `count == 0` returns before the kernel touches the heap or the fd table,
//!   so it prices the syscall boundary alone and is the floor every other
//!   number is measured against.
//! - An invalid descriptor is answered from the fd table, which prices what a
//!   call does once however many descriptors it carries.
//! - Ready descriptors take the path where the device reports readiness at
//!   registration time, so nothing is ever added to a poller list.
//! - Idle descriptors take the path where every descriptor is pushed onto its
//!   device's poller list and removed again before the call returns.
//!
//! A bound UDP socket is polled too. It is the one descriptor kind that
//! registers and unregisters without needing a peer, so it is what covers the
//! socket path here rather than a networked test.
//!
//! One case is a check rather than a measurement: a pipe filled to capacity
//! must stop reporting its write end writable, and start again once a reader
//! takes some. That is the writable half of `poll`, which nothing else here
//! exercises.
//!
//! It also times a clock read and a context switch, because both turned out to
//! cost more than everything above.
//!
//! `-l` mirrors the report to `/dev/klog`, which is how a headless run is read.

use std::io::Write;
use std::time::Instant;

use edos_lib::io::{PollState, SelectFd, close, poll};
use edos_lib::net::{self, SockAddrIn};
use edos_lib::process::{pipe, read, sched_yield, set_nonblocking, write};

const COUNTS: [usize; 5] = [1, 2, 4, 16, 64];

/// Report sink: stdout, and optionally the kernel log as well.
struct Out {
    klog: Option<std::fs::File>,
}

impl Out {
    fn new(enabled: bool) -> Self {
        Self {
            klog: enabled
                .then(|| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/klog")
                        .ok()
                })
                .flatten(),
        }
    }

    fn line(&mut self, text: &str) {
        println!("{text}");
        if let Some(klog) = &mut self.klog {
            let _ = writeln!(klog, "{text}");
        }
    }
}

/// What a pipe holds before a writer has to wait, from
/// `kernel/src/thread/pipe.rs`. The fill loop below asserts against it, so a
/// change to one without the other is a failure rather than a silent drift.
const PIPE_CAPACITY: usize = 64 * 1024;

fn readable() -> PollState {
    PollState {
        readable: true,
        ..PollState::default()
    }
}

fn writable() -> PollState {
    PollState {
        writable: true,
        ..PollState::default()
    }
}

fn fds_for(read_ends: &[u64]) -> Vec<SelectFd> {
    read_ends
        .iter()
        .map(|fd| SelectFd {
            fd: *fd,
            interests: readable(),
            result: PollState::default(),
        })
        .collect()
}

/// Nanoseconds per call, averaged over `iters` calls.
fn time_poll(fds: &mut [SelectFd], iters: u32) -> f64 {
    // Warm the path so the first call's page faults and cache misses do not
    // land in the measurement.
    for _ in 0..64 {
        poll(fds, 0);
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        poll(fds, 0);
    }
    t0.elapsed().as_nanos() as f64 / iters as f64
}

fn main() {
    let mut iters: u32 = 20_000;
    let mut klog = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-l" => klog = true,
            other => match other.parse() {
                Ok(n) => iters = n,
                Err(_) => {
                    eprintln!("usage: pollbench [iterations] [-l]");
                    std::process::exit(2);
                }
            },
        }
    }
    let mut out = Out::new(klog);

    let mut empty: [SelectFd; 0] = [];
    let floor = time_poll(&mut empty, iters);
    out.line(&format!(
        "pollbench {iters} iters, syscall floor (count=0) {floor:.0} ns"
    ));

    // `clock_gettime` is one HPET counter read wrapped in the same syscall
    // boundary the floor measures, so the difference prices a single read of
    // the clock the poll timeout path consults.
    for _ in 0..64 {
        edos_lib::time::clock_gettime_nanos();
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        edos_lib::time::clock_gettime_nanos();
    }
    let clock = t0.elapsed().as_nanos() as f64 / iters as f64;
    out.line(&format!(
        "pollbench clock_gettime {clock:.0} ns, so one HPET read {:.0} ns",
        clock - floor
    ));

    // sched_yield on an otherwise idle CPU returns to the same thread, so it
    // prices the switch bookkeeping rather than a real handover. The scheduler
    // stamps run time from the same clock on the way out. `switchbench` is
    // where the handover cases live.
    for _ in 0..64 {
        sched_yield();
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        sched_yield();
    }
    let yielded = t0.elapsed().as_nanos() as f64 / iters as f64;
    out.line(&format!("pollbench sched_yield {yielded:.0} ns"));

    // What a call costs before any descriptor is looked at.
    //
    // An invalid descriptor is answered from the fd table alone -- nothing is
    // registered and no device lock is taken -- so what is left is the work
    // every call does once: copying the array in and out, taking the fd table,
    // and building the poll set.
    //
    // Do not try to price the allocations by comparing this against a
    // descriptor that reports a fixed readiness. `stdout` is not one: in a
    // terminal it is a PTY slave, which takes the PTY lock and registers like
    // any other device, and the terminal is redrawing while the measurement
    // runs. Both this line and that comparison move by more than half their
    // own value between runs, so treat single readings of either as noise.
    let mut invalid = [SelectFd {
        fd: 9999,
        interests: readable(),
        result: PollState::default(),
    }];
    let invalid_ns = time_poll(&mut invalid, iters);
    if !invalid[0].result.invalid {
        out.line("pollbench: fd 9999 did not report invalid");
        std::process::exit(1);
    }
    out.line(&format!(
        "pollbench 1 invalid fd {invalid_ns:.0} ns, so fixed cost {:.0} ns",
        invalid_ns - floor
    ));

    // A bound UDP socket with nothing to receive is never readable, so it
    // takes the registration path: the socket keeps a poller for the length of
    // the call and drops it again on the way out. That is the only descriptor
    // kind needing no peer to exercise, which is what makes it worth a line
    // here rather than a networked test.
    match net::create_udp_socket() {
        Ok(sock) => {
            let addr = SockAddrIn::new([0, 0, 0, 0], 0);
            if net::bind(sock, &addr).is_err() {
                out.line("pollbench: could not bind a UDP socket");
                std::process::exit(1);
            }
            let mut sock_fds = [SelectFd {
                fd: sock,
                interests: readable(),
                result: PollState::default(),
            }];
            let sock_ns = time_poll(&mut sock_fds, iters);
            if sock_fds[0].result.readable || sock_fds[0].result.invalid {
                out.line("pollbench: an idle UDP socket reported ready or invalid");
                std::process::exit(1);
            }
            out.line(&format!(
                "pollbench 1 idle UDP socket {sock_ns:.0} ns, register and unregister {:.0} ns",
                sock_ns - invalid_ns
            ));
            net::close(sock);
        }
        Err(()) => out.line("pollbench: could not create a UDP socket"),
    }

    // A full pipe is the writable case: `poll` must stop reporting the write
    // end writable once the ring has no room, and start again the moment a
    // reader takes some. Without that a program that waits for POLLOUT spins,
    // and one that trusts it writes into a pipe that will block.
    //
    // Filling it needs the write end non-blocking, so this covers the fd flag
    // as well: the write that finds no room reports EAGAIN rather than parking
    // forever against a reader that is this same thread.
    {
        let (r, w) = pipe().expect("pipe");
        if set_nonblocking(w, true) < 0 {
            out.line("pollbench: could not set O_NONBLOCK on a pipe write end");
            std::process::exit(1);
        }
        let block = [b'x'; 4096];
        let mut filled = 0usize;
        loop {
            let n = write(w, &block);
            if n < 0 {
                break;
            }
            filled += n as usize;
            if filled > 1 << 20 {
                out.line("pollbench: a pipe took more than a megabyte, so it is unbounded");
                std::process::exit(1);
            }
        }
        if filled != PIPE_CAPACITY {
            out.line(&format!(
                "pollbench: a pipe took {filled} bytes, expected {PIPE_CAPACITY}"
            ));
            std::process::exit(1);
        }

        let mut full = [SelectFd {
            fd: w,
            interests: writable(),
            result: PollState::default(),
        }];
        let full_ns = time_poll(&mut full, iters);
        if full[0].result.writable {
            out.line("pollbench: a full pipe reported writable");
            std::process::exit(1);
        }
        out.line(&format!(
            "pollbench 1 full pipe {full_ns:.0} ns, not writable at {filled} bytes"
        ));

        let mut drain = [0u8; 4096];
        if read(r, &mut drain) <= 0 {
            out.line("pollbench: could not drain a full pipe");
            std::process::exit(1);
        }
        full[0].result = PollState::default();
        poll(&mut full, 0);
        if !full[0].result.writable {
            out.line("pollbench: a drained pipe did not report writable");
            std::process::exit(1);
        }
        out.line("pollbench pipe POLLOUT ok: full is not writable, drained is");

        close(r);
        close(w);
    }

    let max = *COUNTS.iter().max().unwrap();

    // One pipe per descriptor. The ready set gets a byte written into it and
    // never drained, so every poll finds it readable; the idle set stays empty.
    let mut ready_pairs = Vec::new();
    let mut idle_pairs = Vec::new();
    for _ in 0..max {
        let (r, w) = pipe().expect("pipe");
        write(w, b"x");
        ready_pairs.push((r, w));
        idle_pairs.push(pipe().expect("pipe"));
    }

    let ready_reads: Vec<u64> = ready_pairs.iter().map(|(r, _)| *r).collect();
    let idle_reads: Vec<u64> = idle_pairs.iter().map(|(r, _)| *r).collect();

    out.line("pollbench   n   ready ns  per-fd    idle ns  per-fd   churn/fd");
    for n in COUNTS {
        let mut ready_fds = fds_for(&ready_reads[..n]);
        let mut idle_fds = fds_for(&idle_reads[..n]);

        let ready = time_poll(&mut ready_fds, iters);
        let idle = time_poll(&mut idle_fds, iters);

        if ready_fds.iter().any(|f| !f.result.readable) {
            out.line("pollbench: a ready descriptor did not report readable");
            std::process::exit(1);
        }
        if idle_fds
            .iter()
            .any(|f| f.result.readable || f.result.invalid)
        {
            out.line("pollbench: an idle descriptor reported ready or invalid");
            std::process::exit(1);
        }

        let n_f = n as f64;
        out.line(&format!(
            "pollbench {n:3}   {ready:8.0}  {:6.0}   {idle:8.0}  {:6.0}   {:8.0}",
            (ready - floor) / n_f,
            (idle - floor) / n_f,
            (idle - ready) / n_f,
        ));
    }
    out.line("pollbench total done");

    for (r, w) in ready_pairs.into_iter().chain(idle_pairs) {
        close(r);
        close(w);
    }
}
