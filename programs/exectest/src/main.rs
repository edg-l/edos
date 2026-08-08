//! Checks what `execve` must preserve and what it must replace.
//!
//! Three stages in one binary. The driver spawns a child (stage 1), the child
//! sets up state and re-executes itself (stage 2), and stage 2 reports what
//! survived through its exit code, which the driver then checks. Descriptors do
//! not cross `spawn`, so everything that has to survive the exec is created by
//! the child itself.

use std::env;
use std::fs::{self, File};
use std::os::fd::AsRawFd;

use edos_lib::process::{self, F_GETFD};

const SELF_PATH: &str = "/bin/exectest";
const DATA_PATH: &str = "/tmp/exectest_data";

// Exit codes stage 2 reports with.
const OK: i32 = 0;
const BAD_ARGS: i32 = 21;
const BAD_PID: i32 = 22;
const BAD_INHERITED_FD: i32 = 23;
const CLOEXEC_FD_SURVIVED: i32 = 24;
const BAD_CWD: i32 = 25;
const EXEC_RETURNED: i32 = 26;

fn fail(test: &str, msg: &str) -> ! {
    eprintln!("FAIL {}: {}", test, msg);
    std::process::exit(1);
}

fn pass(test: &str, detail: &str) {
    println!("PASS {}: {}", test, detail);
}

fn describe(code: i32) -> &'static str {
    match code {
        OK => "ok",
        BAD_ARGS => "argv did not survive the exec",
        BAD_PID => "pid changed across the exec",
        BAD_INHERITED_FD => "an inheritable fd did not survive",
        CLOEXEC_FD_SURVIVED => "a close-on-exec fd survived",
        BAD_CWD => "cwd changed across the exec",
        EXEC_RETURNED => "execve returned instead of replacing the image",
        _ => "unknown failure",
    }
}

/// Stage 1: set up process state, then replace this image with stage 2.
fn stage1() -> ! {
    let inherited = File::open(DATA_PATH).unwrap_or_else(|_| std::process::exit(BAD_INHERITED_FD));
    let cloexec = File::open(DATA_PATH).unwrap_or_else(|_| std::process::exit(BAD_INHERITED_FD));
    let inherited_fd = inherited.as_raw_fd() as u64;
    let cloexec_fd = cloexec.as_raw_fd() as u64;

    if process::set_cloexec(cloexec_fd, true) < 0 {
        std::process::exit(CLOEXEC_FD_SURVIVED);
    }

    // The descriptors must outlive these handles; the exec (or the process's
    // death) is what closes them.
    std::mem::forget(inherited);
    std::mem::forget(cloexec);

    if env::set_current_dir("/tmp").is_err() {
        std::process::exit(BAD_CWD);
    }

    let pid = process::getpid();
    process::execve(
        SELF_PATH,
        &[
            "stage2",
            &pid.to_string(),
            &inherited_fd.to_string(),
            &cloexec_fd.to_string(),
        ],
        &[],
    );

    // execve only returns on failure.
    std::process::exit(EXEC_RETURNED);
}

/// Stage 2: the new image. Everything checked here crossed the exec.
fn stage2(args: &[String]) -> ! {
    if args.len() < 5 {
        std::process::exit(BAD_ARGS);
    }

    let expected_pid: u64 = args[2].parse().unwrap_or(0);
    let inherited_fd: u64 = args[3].parse().unwrap_or(0);
    let cloexec_fd: u64 = args[4].parse().unwrap_or(0);

    // The pid surviving is the whole difference between exec and spawn.
    if process::getpid() != expected_pid {
        std::process::exit(BAD_PID);
    }

    // An inheritable descriptor is still open and still refers to the same file.
    let mut buf = [0u8; 5];
    let n = edos_lib::io::pread(inherited_fd, &mut buf, 0);
    if n != 5 || &buf != b"hello" {
        std::process::exit(BAD_INHERITED_FD);
    }

    // A close-on-exec descriptor is gone.
    if process::fcntl(cloexec_fd, F_GETFD, 0) >= 0 {
        std::process::exit(CLOEXEC_FD_SURVIVED);
    }

    // cwd is process state, not image state.
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if cwd != "/tmp" {
        std::process::exit(BAD_CWD);
    }

    std::process::exit(OK);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("stage1") => stage1(),
        Some("stage2") => stage2(&args),
        // Replaces this image with a program whose exit status the driver then
        // reads, proving the status belongs to the process rather than to
        // whoever called exec.
        // Exec from a multithreaded process: the siblings must be terminated
        // and off the address space before it is torn down.
        Some("stage4") => {
            for i in 0..4 {
                std::thread::spawn(move || {
                    // These siblings keep entering the kernel, so they notice
                    // the kill at a syscall boundary; stage 5 covers the ones
                    // that never reach one.
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        std::hint::black_box(i);
                    }
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            process::execve("/bin/true", &["true"], &[]);
            std::process::exit(EXEC_RETURNED);
        }
        // The same, with siblings that make no syscalls at all. They can only
        // notice the kill on a timer tick out of user code, which is the one
        // path that lets exec quiesce a spinning thread instead of refusing.
        Some("stage5") => {
            for i in 0..4 {
                std::thread::spawn(move || {
                    let mut n = i as u64;
                    loop {
                        n = std::hint::black_box(n).wrapping_add(1);
                    }
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            process::execve("/bin/true", &["true"], &[]);
            std::process::exit(EXEC_RETURNED);
        }
        Some("stage3") => {
            process::execve("/bin/true", &["true"], &[]);
            std::process::exit(EXEC_RETURNED);
        }
        _ => {}
    }

    // -------------------------------------------------------------------
    // Test 1: a failed exec returns, and leaves this process running
    // -------------------------------------------------------------------
    if process::execve("/bin/definitely_not_here", &["x"], &[]) >= 0 {
        fail("test 1", "execve of a missing file reported success");
    }
    if process::execve("/etc", &["x"], &[]) >= 0 {
        fail("test 1", "execve of a directory reported success");
    }
    let pid_before = process::getpid();
    if pid_before == 0 {
        fail("test 1", "getpid after a failed execve returned 0");
    }
    pass("test 1", "a failed execve returns with the process intact");

    // -------------------------------------------------------------------
    // Test 2: pid, fds and cwd across a successful exec
    // -------------------------------------------------------------------
    fs::write(DATA_PATH, b"hello world")
        .unwrap_or_else(|e| fail("test 2", &format!("write {}: {}", DATA_PATH, e)));

    let child = process::spawn(SELF_PATH, &["stage1"], 0, 1, 2);
    if child == u64::MAX {
        fail("test 2", "spawn of stage 1 failed");
    }
    let code = process::waitpid(child);
    if code != OK {
        fail(
            "test 2",
            &format!("stage 2 exited {}: {}", code, describe(code)),
        );
    }
    pass(
        "test 2",
        "pid, cwd and inheritable fds survived; close-on-exec fd did not",
    );

    // -------------------------------------------------------------------
    // Test 3: the exec'd image's exit status is the process's exit status
    // -------------------------------------------------------------------
    let child = process::spawn(SELF_PATH, &["stage3"], 0, 1, 2);
    if child == u64::MAX {
        fail("test 3", "spawn failed");
    }
    let code = process::waitpid(child);
    if code != 0 {
        fail(
            "test 3",
            &format!("expected the exec'd /bin/true to exit 0, got {}", code),
        );
    }
    pass("test 3", "the exec'd image's exit status is the process's");

    // -------------------------------------------------------------------
    // Test 4: exec from a multithreaded process
    // -------------------------------------------------------------------
    let child = process::spawn(SELF_PATH, &["stage4"], 0, 1, 2);
    if child == u64::MAX {
        fail("test 4", "spawn failed");
    }
    let code = process::waitpid(child);
    if code != 0 {
        fail(
            "test 4",
            &format!(
                "exec from a 5-thread process exited {}: {}",
                code,
                describe(code)
            ),
        );
    }
    pass(
        "test 4",
        "exec from a multithreaded process replaced the image",
    );

    // -------------------------------------------------------------------
    // Test 5: exec from a process whose siblings never enter the kernel
    // -------------------------------------------------------------------
    let child = process::spawn(SELF_PATH, &["stage5"], 0, 1, 2);
    if child == u64::MAX {
        fail("test 5", "spawn failed");
    }
    let code = process::waitpid(child);
    if code != 0 {
        fail(
            "test 5",
            &format!(
                "exec with 4 user-spinning siblings exited {}: {}",
                code,
                describe(code)
            ),
        );
    }
    pass("test 5", "exec quiesced siblings that make no syscalls");

    let _ = fs::remove_file(DATA_PATH);
    println!("exectest: all tests passed");
    std::process::exit(0);
}
