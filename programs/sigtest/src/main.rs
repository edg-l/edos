//! Exercises the parts of the signal subsystem nothing else reaches.
//!
//! Each case proves one mechanism that did not exist before: a handler running
//! on the process's own stack and returning through `sigreturn`, a stop signal
//! suspending a process and `SIGCONT` resuming it, a signal aimed at a process
//! *group* reaching every member, and `SIGPIPE` terminating a producer whose
//! reader is gone.

use std::sync::atomic::{AtomicU32, Ordering};

use edos_lib::process::{self, ChildState, SIGCONT, SIGINT, SIGTERM, SIGTSTP, waitpid_untraced};

fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

static CAUGHT: AtomicU32 = AtomicU32::new(0);
static LAST_SIGNAL: AtomicU32 = AtomicU32::new(0);

extern "C" fn record(signum: u32) {
    CAUGHT.fetch_add(1, Ordering::SeqCst);
    LAST_SIGNAL.store(signum, Ordering::SeqCst);
}

fn pass(name: &str, detail: &str) {
    println!("PASS {name}: {detail}");
}

fn fail(name: &str, detail: &str) -> ! {
    println!("FAIL {name}: {detail}");
    std::process::exit(1);
}

/// A handler runs and the interrupted code carries on.
///
/// The syscall in the middle is the point: its return value has to survive the
/// detour, because the kernel saves and restores the whole context and not
/// just the instruction pointer.
fn handler_runs() {
    if process::signal(SIGINT, record) < 0 {
        fail("handler", "sigaction rejected a handler");
    }

    let me = process::getpid();
    let before = process::getpid();
    if process::kill(me, SIGINT) < 0 {
        fail("handler", "kill of self failed");
    }

    // Delivery is at a syscall return, so one syscall is enough to take it.
    let after = process::getpid();

    if CAUGHT.load(Ordering::SeqCst) != 1 {
        fail("handler", "handler did not run");
    }
    if LAST_SIGNAL.load(Ordering::SeqCst) != SIGINT {
        fail("handler", "handler got the wrong signal number");
    }
    if before != me || after != me {
        fail("handler", "a syscall around the handler returned garbage");
    }
    pass(
        "handler",
        "ran once, and the interrupted syscall still returned",
    );
}

/// A second delivery proves `sigreturn` restored the mask rather than leaving
/// the signal blocked forever.
fn handler_repeats() {
    let me = process::getpid();
    process::kill(me, SIGINT);
    let _ = process::getpid();

    if CAUGHT.load(Ordering::SeqCst) != 2 {
        fail("mask", "the signal stayed blocked after the first handler");
    }
    pass("mask", "sigreturn restored the blocked mask");
}

/// An ignored signal does not run the handler and does not kill.
fn ignore_works() {
    process::sys_sigaction(SIGTERM, process::SIG_IGN as u64);
    let me = process::getpid();
    process::kill(me, SIGTERM);
    let _ = process::getpid();
    pass(
        "ignore",
        "SIG_IGN survived a signal whose default is terminate",
    );
}

/// A stopped child is reported as stopped, resumes on SIGCONT, and only then
/// runs to completion.
fn stop_and_continue() {
    let pid = process::fork();
    if pid < 0 {
        fail("stop", "fork failed");
    }
    if pid == 0 {
        // Long enough that the parent's stop lands while this is running.
        for _ in 0..400 {
            sleep_ms(5);
        }
        std::process::exit(7);
    }
    let child = pid as u64;

    sleep_ms(50);
    if process::kill(child, SIGTSTP) < 0 {
        fail("stop", "could not send SIGTSTP");
    }

    // The stop takes effect at the child's next syscall boundary.
    let mut stopped = false;
    for _ in 0..100 {
        sleep_ms(10);
        if waitpid_untraced(child) == Some(ChildState::Stopped) {
            stopped = true;
            break;
        }
    }
    if !stopped {
        fail("stop", "child never reported as stopped");
    }
    pass("stop", "SIGTSTP suspended the child and waitpid saw it");

    // A stopped process makes no progress: it must still be stopped after a
    // delay long enough for it to have finished otherwise.
    sleep_ms(120);
    if waitpid_untraced(child) != Some(ChildState::Stopped) {
        fail("stop", "child resumed on its own");
    }

    if process::kill(child, SIGCONT) < 0 {
        fail("cont", "could not send SIGCONT");
    }
    if process::waitpid(child) != 7 {
        fail("cont", "child did not resume and exit cleanly");
    }
    pass("cont", "SIGCONT resumed it and it ran to completion");
}

/// One signal aimed at a group reaches every process in it.
fn group_delivery() {
    let mut children = Vec::new();
    let mut group = 0u64;

    for _ in 0..3 {
        let pid = process::fork();
        if pid < 0 {
            fail("group", "fork failed");
        }
        if pid == 0 {
            for _ in 0..400 {
                sleep_ms(5);
            }
            std::process::exit(0);
        }
        let child = pid as u64;
        // The first child leads the group; the rest join it. This is exactly
        // what a shell does for a pipeline.
        if group == 0 {
            group = child;
        }
        process::setpgid(child, group);
        children.push(child);
    }

    sleep_ms(60);
    // Negative pid: the group named by its magnitude.
    if process::kill_group(group, SIGTERM) < 0 {
        fail("group", "kill of the group failed");
    }

    for child in &children {
        let status = process::waitpid(*child);
        if status != 128 + SIGTERM as i32 {
            fail("group", "a group member survived the signal");
        }
    }
    pass("group", "one signal reached all three members of the group");
}

/// A write to a pipe nobody is reading terminates the writer instead of
/// buffering forever.
fn sigpipe_terminates() {
    let Some((read_fd, write_fd)) = process::pipe() else {
        fail("sigpipe", "could not create a pipe");
    };

    let pid = process::fork();
    if pid < 0 {
        fail("sigpipe", "fork failed");
    }
    if pid == 0 {
        process::close(read_fd);
        // Without SIGPIPE this never ends and grows the kernel heap.
        let buf = [b'x'; 4096];
        loop {
            process::write(write_fd, &buf);
        }
    }

    process::close(read_fd);
    process::close(write_fd);

    let status = process::waitpid(pid as u64);
    if status != 128 + 13 {
        fail("sigpipe", "writer was not killed by SIGPIPE");
    }
    pass("sigpipe", "a write with no reader raised SIGPIPE");
}

fn main() {
    handler_runs();
    handler_repeats();
    ignore_works();
    stop_and_continue();
    group_delivery();
    sigpipe_terminates();
    println!("sigtest: all cases passed");
}
