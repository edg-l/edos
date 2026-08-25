//! Process management, pipe support, and program spawning.

use crate::sys::{self, Errno};
use std::ffi::CString;

/// Create a PTY pair for terminal emulation.
/// Returns (master_fd, slave_fd) on success, or None on error.
pub fn openpty() -> Option<(u64, u64)> {
    let mut ptyfd = [0u64; 2];
    let result = unsafe { sys::syscall1(sys::SYS_OPENPTY, ptyfd.as_mut_ptr() as u64) };
    if result == 0 {
        Some((ptyfd[0], ptyfd[1]))
    } else {
        None
    }
}

/// Create a pipe for inter-process communication.
/// Returns (read_fd, write_fd) on success, or None on error.
pub fn pipe() -> Option<(u64, u64)> {
    let mut pipefd = [0u64; 2];
    let result = unsafe { sys::syscall1(sys::SYS_PIPE, pipefd.as_mut_ptr() as u64) };
    if result == 0 {
        Some((pipefd[0], pipefd[1]))
    } else {
        None
    }
}

/// Close a file descriptor.
pub fn close(fd: u64) -> i32 {
    unsafe { sys::syscall1(sys::SYS_CLOSE, fd) as i32 }
}

/// Read from a file descriptor, answering how many bytes landed in `buf`.
pub fn read(fd: u64, buf: &mut [u8]) -> Result<usize, Errno> {
    let ret =
        unsafe { sys::syscall3(sys::SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) };
    sys::sys_result(ret).map(|n| n as usize)
}

/// Write to a file descriptor, answering how many bytes of `buf` were taken.
pub fn write(fd: u64, buf: &[u8]) -> Result<usize, Errno> {
    let ret = unsafe { sys::syscall3(sys::SYS_WRITE, fd, buf.as_ptr() as u64, buf.len() as u64) };
    sys::sys_result(ret).map(|n| n as usize)
}

/// Duplicate a file descriptor, assigning the lowest unused fd.
pub fn dup(fd: u64) -> Result<u64, Errno> {
    sys::sys_result(unsafe { sys::syscall1(sys::SYS_DUP, fd) })
}

/// Duplicate a file descriptor onto `new_fd`, closing whatever was there.
pub fn dup2(old_fd: u64, new_fd: u64) -> Result<u64, Errno> {
    sys::sys_result(unsafe { sys::syscall2(sys::SYS_DUP2, old_fd, new_fd) })
}

/// Get the current process ID.
pub fn getpid() -> u64 {
    unsafe { sys::syscall1(sys::SYS_GETPID, 0) }
}

/// Replace the current process image with `path`, keeping the pid.
///
/// Returns only on failure, and then the reason. Descriptors marked
/// close-on-exec are closed; everything else, along with the cwd and pid,
/// carries into the new image. A kernel that answered success without
/// replacing the image is reported as [`Errno::UNKNOWN`], a code it never
/// sends of its own accord.
pub fn execve(path: &str, args: &[&str], env: &[&str]) -> Errno {
    let mut path_buf = std::vec::Vec::with_capacity(path.len() + 1);
    path_buf.extend_from_slice(path.as_bytes());
    path_buf.push(0);

    let arg_bufs: std::vec::Vec<std::vec::Vec<u8>> = args
        .iter()
        .map(|a| {
            let mut b = std::vec::Vec::with_capacity(a.len() + 1);
            b.extend_from_slice(a.as_bytes());
            b.push(0);
            b
        })
        .collect();
    let mut argv: std::vec::Vec<*const u8> = arg_bufs.iter().map(|b| b.as_ptr()).collect();
    argv.push(core::ptr::null());

    let env_bufs: std::vec::Vec<std::vec::Vec<u8>> = env
        .iter()
        .map(|e| {
            let mut b = std::vec::Vec::with_capacity(e.len() + 1);
            b.extend_from_slice(e.as_bytes());
            b.push(0);
            b
        })
        .collect();
    let mut envp: std::vec::Vec<*const u8> = env_bufs.iter().map(|b| b.as_ptr()).collect();
    envp.push(core::ptr::null());

    let ret = unsafe {
        sys::syscall3(
            sys::SYS_EXECVE,
            path_buf.as_ptr() as u64,
            argv.as_ptr() as u64,
            envp.as_ptr() as u64,
        )
    };
    sys::sys_result(ret).err().unwrap_or(Errno::UNKNOWN)
}

/// `fcntl` commands supported by the kernel.
pub const F_DUPFD: u64 = 0;
pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;
pub const F_GETFL: u64 = 3;
pub const F_SETFL: u64 = 4;
pub const F_DUPFD_CLOEXEC: u64 = 1030;
pub const FD_CLOEXEC: u64 = 1;

/// `fcntl(fd, cmd, arg)`.
pub fn fcntl(fd: u64, cmd: u64, arg: u64) -> Result<u64, Errno> {
    sys::sys_result(unsafe { sys::syscall3(sys::SYS_FCNTL, fd, cmd, arg) })
}

/// Mark or unmark a descriptor close-on-exec.
pub fn set_cloexec(fd: u64, on: bool) -> Result<(), Errno> {
    fcntl(fd, F_SETFD, if on { FD_CLOEXEC } else { 0 }).map(|_| ())
}

/// Put a descriptor in or out of non-blocking mode, so a read with nothing to
/// read and a write with nowhere to put it fail with `EAGAIN` instead of
/// waiting.
///
/// `O_NONBLOCK` is the only status flag `F_SETFL` can change, so the read back
/// costs nothing but keeps this from clearing one that is later added.
pub fn set_nonblocking(fd: u64, on: bool) -> Result<(), Errno> {
    let flags = fcntl(fd, F_GETFL, 0)?;
    let new = if on {
        flags | crate::io::O_NONBLOCK
    } else {
        flags & !crate::io::O_NONBLOCK
    };
    fcntl(fd, F_SETFL, new).map(|_| ())
}

/// Real user id of the calling process.
pub fn getuid() -> u32 {
    unsafe { sys::syscall0(sys::SYS_GETUID) as u32 }
}

/// Real group id of the calling process.
pub fn getgid() -> u32 {
    unsafe { sys::syscall0(sys::SYS_GETGID) as u32 }
}

/// Name for a user or group id, or `None` when there is no name for it.
///
/// There is no password or group database on this system and no way to become
/// anything but id 0, so the table is the single identity the kernel hands out.
/// This is the one place to replace when a `/etc/passwd` exists.
pub fn id_name(id: u32) -> Option<&'static str> {
    match id {
        0 => Some("root"),
        _ => None,
    }
}

/// Spawn a new process with redirected I/O.
///
/// # Arguments
/// * `path` - Path to the executable
/// * `args` - Command line arguments
/// * `stdin_fd` - File descriptor for stdin (or 0 for default)
/// * `stdout_fd` - File descriptor for stdout (or 1 for default)
/// * `stderr_fd` - File descriptor for stderr (or 2 for default)
///
/// # Returns
/// The child's process id.
pub fn spawn(
    path: &str,
    args: &[&str],
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
) -> Result<u64, Errno> {
    // Create null-terminated path
    let mut path_buf = Vec::with_capacity(path.len() + 1);
    path_buf.extend_from_slice(path.as_bytes());
    path_buf.push(0);

    // Create null-terminated args
    let mut argv_storage: Vec<Vec<u8>> = Vec::with_capacity(args.len());
    let mut argv_ptrs: Vec<*const u8> = Vec::with_capacity(args.len() + 1);

    for &arg in args {
        let mut buf = Vec::with_capacity(arg.len() + 1);
        buf.extend_from_slice(arg.as_bytes());
        buf.push(0);
        argv_ptrs.push(buf.as_ptr());
        argv_storage.push(buf);
    }
    argv_ptrs.push(core::ptr::null());

    let argv_ptr = if argv_ptrs.is_empty() {
        core::ptr::null()
    } else {
        argv_ptrs.as_ptr()
    };

    let ret = unsafe {
        sys::syscall5(
            sys::SYS_SPAWN,
            path_buf.as_ptr() as u64,
            argv_ptr as u64,
            stdin_fd,
            stdout_fd,
            stderr_fd,
        )
    };
    sys::sys_result(ret)
}

/// Spawn `path` with redirected I/O, passing the caller's environment on.
///
/// Same as [`spawn`] except that the child inherits the environment instead of
/// starting with an empty one, which is how a session-wide setting such as `TZ`
/// reaches the programs `edos-init` starts.
pub fn spawn_with_env(
    path: &str,
    args: &[&str],
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
) -> Result<u64, Errno> {
    spawn_envp(
        path,
        args,
        &current_env_strings(),
        stdin_fd,
        stdout_fd,
        stderr_fd,
    )
}

/// Same as [`spawn_with_env`] except the child's environment is given rather
/// than inherited.
///
/// A server that gives each client its own `TERM` and `HOME` needs this:
/// the environment belongs to the connection, and one process may be serving
/// several at once, so it cannot be carried in the caller's own environment.
///
/// Each entry is a `KEY=VALUE` pair; entries containing a NUL are dropped.
pub fn spawn_with_envp(
    path: &str,
    args: &[&str],
    env: &[&str],
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
) -> Result<u64, Errno> {
    let strings: Vec<Vec<u8>> = env
        .iter()
        .filter(|e| !e.as_bytes().contains(&0))
        .map(|e| {
            let mut buf = Vec::with_capacity(e.len() + 1);
            buf.extend_from_slice(e.as_bytes());
            buf.push(0);
            buf
        })
        .collect();
    spawn_envp(path, args, &strings, stdin_fd, stdout_fd, stderr_fd)
}

/// The one `SYS_SPAWN2` call site: `envp` arrives as NUL-terminated
/// `KEY=VALUE` byte strings, whoever assembled them.
fn spawn_envp(
    path: &str,
    args: &[&str],
    env_strings: &[Vec<u8>],
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
) -> Result<u64, Errno> {
    let Ok(c_path) = CString::new(path) else {
        return Err(Errno::EINVAL);
    };
    let c_args: Vec<CString> = args.iter().filter_map(|a| CString::new(*a).ok()).collect();
    let mut argv_ptrs: Vec<*const u8> = c_args.iter().map(|c| c.as_ptr() as *const u8).collect();
    argv_ptrs.push(core::ptr::null());

    let mut envp_ptrs: Vec<*const u8> = env_strings.iter().map(|s| s.as_ptr()).collect();
    envp_ptrs.push(core::ptr::null());

    let spawn_args = SpawnArgs {
        path: c_path.as_ptr() as *const u8,
        argv: argv_ptrs.as_ptr(),
        envp: envp_ptrs.as_ptr(),
        stdin_fd,
        stdout_fd,
        stderr_fd,
    };
    sys::sys_result(unsafe {
        sys::syscall1(sys::SYS_SPAWN2, &spawn_args as *const SpawnArgs as u64)
    })
}

/// The caller's environment as NUL-terminated `KEY=VALUE` byte strings, ready
/// for a `SYS_SPAWN2` `envp`. Entries containing a NUL are dropped, since they
/// cannot be represented as C strings.
fn current_env_strings() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (key, val) in std::env::vars_os() {
        let key_bytes = key.as_encoded_bytes();
        let val_bytes = val.as_encoded_bytes();
        if key_bytes.contains(&0) || val_bytes.contains(&0) {
            continue;
        }
        let mut entry = Vec::with_capacity(key_bytes.len() + val_bytes.len() + 2);
        entry.extend_from_slice(key_bytes);
        entry.push(b'=');
        entry.extend_from_slice(val_bytes);
        entry.push(0);
        out.push(entry);
    }
    out
}

/// Wait for a process to exit (blocking).
/// Returns the exit code of the child process, or -1 on failure.
pub fn waitpid(pid: u64) -> i32 {
    let mut status: i32 = -1;
    let ret = unsafe {
        sys::syscall3(
            sys::SYS_WAIT_PID,
            pid,
            WAIT_BLOCK,
            &mut status as *mut i32 as u64,
        )
    };
    if sys::is_err(ret) { -1 } else { status }
}

/// `waitpid` flags.
pub const WAIT_BLOCK: u64 = 1;
/// Report a child that stopped as well as one that exited.
pub const WAIT_UNTRACED: u64 = 2;

/// Status the kernel reports for a stopped child. Not a possible exit code,
/// which is a byte.
pub const STATUS_STOPPED: i32 = 0x1_0000;

/// What became of a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildState {
    Exited(i32),
    Stopped,
}

/// Check on a child without blocking, noticing a stop as well as an exit.
///
/// A shell needs both: a job that stopped is still a job, and one that exited
/// is not. Plain [`waitpid_nonblocking`] cannot tell them apart because a
/// stopped child never exits on its own.
pub fn waitpid_untraced(pid: u64) -> Option<ChildState> {
    wait_untraced(pid, WAIT_UNTRACED)
}

/// Wait for a child to exit *or* stop, blocking until one of the two happens.
///
/// This is the wait a shell does on a foreground job: it must come back both
/// when the job finishes and when Ctrl+Z suspends it, and it must not spin in
/// the meantime.
pub fn waitpid_untraced_blocking(pid: u64) -> Option<ChildState> {
    wait_untraced(pid, WAIT_BLOCK | WAIT_UNTRACED)
}

fn wait_untraced(pid: u64, flags: u64) -> Option<ChildState> {
    let mut status: i32 = -1;
    let ret = unsafe {
        sys::syscall3(
            sys::SYS_WAIT_PID,
            pid,
            flags,
            &mut status as *mut i32 as u64,
        )
    };
    if ret != pid {
        return None;
    }
    Some(if status == STATUS_STOPPED {
        ChildState::Stopped
    } else {
        ChildState::Exited(status)
    })
}

/// Place `pid` in process group `pgid`.
///
/// `pid` of 0 means the caller, `pgid` of 0 means "lead a new group". A shell
/// uses both: the first process of a job leads a group and the rest join it,
/// which is what makes one Ctrl+C stop a whole pipeline.
pub fn setpgid(pid: u64, pgid: u64) -> Result<(), Errno> {
    sys::sys_result(unsafe { sys::syscall2(sys::SYS_SETPGID, pid, pgid) }).map(|_| ())
}

/// The process group of `pid`, or of the caller when `pid` is 0.
pub fn getpgid(pid: u64) -> Result<u64, Errno> {
    sys::sys_result(unsafe { sys::syscall1(sys::SYS_GETPGID, pid) })
}

/// Hand the terminal on `fd` to process group `pgid`.
///
/// The line discipline aims Ctrl+C and Ctrl+Z at whichever group holds the
/// terminal, so this is what "foreground" means.
pub fn tcsetpgrp(fd: u64, pgid: u64) -> Result<(), Errno> {
    sys::sys_result(unsafe { sys::syscall2(sys::SYS_TCSETPGRP, fd, pgid) }).map(|_| ())
}

/// The process group currently holding the terminal on `fd`.
pub fn tcgetpgrp(fd: u64) -> Result<u64, Errno> {
    sys::sys_result(unsafe { sys::syscall1(sys::SYS_TCGETPGRP, fd) })
}

/// Check if a process has exited without blocking.
/// Returns `Some(exit_code)` if the child exited, `None` if still running.
pub fn waitpid_nonblocking(pid: u64) -> Option<i32> {
    let mut status: i32 = -1;
    let ret = unsafe { sys::syscall3(sys::SYS_WAIT_PID, pid, 0, &mut status as *mut i32 as u64) };
    if ret == pid { Some(status) } else { None }
}

/// A child process connected via a PTY master fd.
pub struct ChildProcess {
    /// Process ID
    pub pid: u64,
    /// PTY master fd - read output from and write input to the child through this
    pub master_fd: u64,
}

impl ChildProcess {
    /// Spawn a program on the far end of a PTY.
    ///
    /// # Arguments
    /// * `path` - Path to the executable (e.g., "/bin/sh")
    /// * `args` - Arguments after argv[0]
    ///
    /// # Returns
    /// A ChildProcess on success, or None on error.
    pub fn spawn_shell(path: &str, args: &[&str]) -> Option<Self> {
        let (master_fd, slave_fd) = openpty()?;

        let pid = spawn_with_env(
            path, args, slave_fd, // shell's stdin
            slave_fd, // shell's stdout
            slave_fd, // shell's stderr
        );

        // Parent closes the slave end regardless of spawn outcome
        close(slave_fd);

        let Ok(pid) = pid else {
            close(master_fd);
            return None;
        };

        Some(Self { pid, master_fd })
    }

    /// Write data to the child's stdin via the PTY master.
    pub fn write(&self, data: &[u8]) -> Result<usize, Errno> {
        write(self.master_fd, data)
    }

    /// Write a string to the child's stdin via the PTY master.
    pub fn write_str(&self, s: &str) -> Result<usize, Errno> {
        self.write(s.as_bytes())
    }

    /// Read data from the child's stdout via the PTY master (non-blocking).
    /// A read with nothing to read answers 0 rather than blocking.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        read(self.master_fd, buf)
    }

    /// Try to read available output as a string.
    pub fn read_available(&self) -> Option<String> {
        let mut buf = [0u8; 4096];
        match self.read(&mut buf) {
            Ok(n) if n > 0 => Some(String::from_utf8_lossy(&buf[..n]).into_owned()),
            _ => None,
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        close(self.master_fd);
        // Note: we don't wait for the child or kill it here
    }
}

/// Arguments structure for SYS_SPAWN2. Must match the kernel's layout exactly:
/// path, argv, envp, stdin_fd, stdout_fd, stderr_fd.
#[repr(C)]
struct SpawnArgs {
    path: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
}

/// Spawn a program with custom fd redirections. Returns Some(pid) on success.
/// Uses SYS_SPAWN2 to pass the current process environment to the child.
pub fn spawn_program_with_fds(
    command: &str,
    args: &[String],
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
) -> Option<u64> {
    let candidates = [
        format!("/bin/{}", command),
        format!("./{}", command),
        format!("/usr/bin/{}", command),
        format!("/{}", command),
    ];

    // Build argv as C strings (don't include command name - kernel adds path as argv[0])
    let mut c_args: Vec<CString> = Vec::with_capacity(args.len());
    for arg in args {
        if let Ok(c) = CString::new(arg.as_str()) {
            c_args.push(c);
        }
    }

    // Build argv pointer array (null-terminated)
    let mut argv_ptrs: Vec<*const u8> = c_args.iter().map(|c| c.as_ptr() as *const u8).collect();
    argv_ptrs.push(std::ptr::null());

    let env_strings = current_env_strings();
    let mut envp_ptrs: Vec<*const u8> = env_strings.iter().map(|s| s.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    for path in &candidates {
        let Ok(c_path) = CString::new(path.as_str()) else {
            continue;
        };
        let spawn_args = SpawnArgs {
            path: c_path.as_ptr() as *const u8,
            argv: argv_ptrs.as_ptr(),
            envp: envp_ptrs.as_ptr(),
            stdin_fd,
            stdout_fd,
            stderr_fd,
        };
        let pid = unsafe { sys::syscall1(sys::SYS_SPAWN2, &spawn_args as *const SpawnArgs as u64) };
        if !sys::is_err(pid) {
            return Some(pid);
        }
    }
    None
}

/// Try to spawn an external program and wait for it to complete.
pub fn spawn_program(command: &str, args: &[String]) {
    if let Some(pid) = spawn_program_with_fds(command, args, 0, 1, 2) {
        waitpid(pid);
    } else {
        eprintln!("Command not found: {}", command);
    }
}

/// How one of a command's three standard descriptors is wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdioSlot {
    /// Follow descriptor `n`'s default: for a pipeline stage that is the pipe
    /// end, otherwise the caller's own descriptor. The index is recorded
    /// rather than resolved so that `2>&1` written before `>file` still
    /// tracks the original standard output, as the shell requires.
    Default(usize),
    /// An already-open descriptor.
    Fd(u64),
}

impl StdioSlot {
    /// Resolve against the descriptors the command would have used with no
    /// redirection at all.
    pub fn resolve(self, defaults: [u64; 3]) -> u64 {
        match self {
            StdioSlot::Default(n) => defaults[n],
            StdioSlot::Fd(fd) => fd,
        }
    }
}

/// Every descriptor following its own default: no redirection.
pub const STDIO_DEFAULT: [StdioSlot; 3] = [
    StdioSlot::Default(0),
    StdioSlot::Default(1),
    StdioSlot::Default(2),
];

/// One stage of a pipeline.
pub struct PipelineStage {
    pub command: String,
    pub args: Vec<String>,
    /// Redirections layered on top of the pipe wiring.
    pub slots: [StdioSlot; 3],
}

/// Spawn a pipeline of commands connected by pipes.
///
/// Returns the pid of every stage, in pipeline order, and does not wait: the
/// caller decides whether the job runs in the foreground and owns putting the
/// stages in one process group. A stage that fails to spawn ends the pipeline,
/// so a short vector means the rest never started.
pub fn spawn_pipeline(stages: &[PipelineStage]) -> Vec<u64> {
    let mut prev_read_fd: Option<u64> = None;
    let mut pids: Vec<u64> = Vec::with_capacity(stages.len());

    for (i, stage) in stages.iter().enumerate() {
        let is_last = i == stages.len() - 1;

        // Create pipe for this stage's output (except the last stage)
        let (read_fd, write_fd) = if !is_last {
            match pipe() {
                Some((r, w)) => (Some(r), Some(w)),
                None => {
                    eprintln!("Failed to create pipe");
                    // Close any still-open read end from the previous stage
                    if let Some(fd) = prev_read_fd {
                        close(fd);
                    }
                    return pids;
                }
            }
        } else {
            (None, None)
        };

        let defaults = [prev_read_fd.unwrap_or(0), write_fd.unwrap_or(1), 2];
        let pid = spawn_program_with_fds(
            &stage.command,
            &stage.args,
            stage.slots[0].resolve(defaults),
            stage.slots[1].resolve(defaults),
            stage.slots[2].resolve(defaults),
        );

        // Close pipe ends the parent no longer needs after spawning
        if let Some(fd) = prev_read_fd {
            close(fd);
        }
        if let Some(fd) = write_fd {
            close(fd);
        }

        let Some(pid) = pid else {
            eprintln!("Command not found: {}", stage.command);
            // Close the read end of the pipe we just created (if any)
            if let Some(fd) = read_fd {
                close(fd);
            }
            return pids;
        };

        pids.push(pid);
        prev_read_fd = read_fd;
    }

    pids
}

/// Fork the calling process (COW).
///
/// Answers the child's pid in the parent and 0 in the child, which is the
/// one place a single return value means two different things and the reason
/// callers match on it rather than test it.
pub fn fork() -> Result<u64, Errno> {
    sys::sys_result(unsafe { sys::syscall0(sys::SYS_FORK) })
}

/// Give up the rest of this thread's timeslice.
///
/// The thread stays runnable, so on a CPU with nothing else Ready the
/// scheduler picks it straight back up.
pub fn sched_yield() {
    unsafe { sys::syscall0(sys::SYS_SCHED_YIELD) };
}

/// What a thread asks the scheduler for. Mirrors the kernel's `SchedAttr` in
/// `syscalls/mod.rs` field for field.
///
/// The two dials are not the same dial. `priority` selects a weight and so a
/// *share* of the CPU, taken from everything else on it. `slice_ns` is a
/// *request*: how long a turn lasts, and so how soon the next one comes. Asking
/// for a shorter slice buys latency at the price of switches and takes
/// bandwidth from nobody.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedAttr {
    /// 0..16, higher is a larger share. 7 is the default.
    pub priority: u32,
    pub _pad: u32,
    /// Nanoseconds of service per pick. The kernel clamps it to the range it
    /// will serve, so read it back to learn what was granted.
    pub slice_ns: u64,
}

/// Set a thread's scheduling attributes. `tid` of 0 is the calling thread.
pub fn sched_setattr(tid: u64, attr: &SchedAttr) -> Result<(), Errno> {
    let ret =
        unsafe { sys::syscall2(sys::SYS_SCHED_SETATTR, tid, attr as *const SchedAttr as u64) };
    sys::sys_result(ret).map(|_| ())
}

/// Read a thread's scheduling attributes. `tid` of 0 is the calling thread.
pub fn sched_getattr(tid: u64) -> Result<SchedAttr, Errno> {
    let mut attr = SchedAttr::default();
    let ret = unsafe {
        sys::syscall2(
            sys::SYS_SCHED_GETATTR,
            tid,
            &mut attr as *mut SchedAttr as u64,
        )
    };
    sys::sys_result(ret).map(|_| attr)
}

pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGKILL: u32 = 9;
pub const SIGPIPE: u32 = 13;
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;

/// Signal dispositions accepted by [`sys_sigaction`].
pub const SIG_DFL: u32 = 0;
pub const SIG_IGN: u32 = 1;

/// `sigprocmask` operations.
pub const SIG_BLOCK: u32 = 0;
pub const SIG_UNBLOCK: u32 = 1;
pub const SIG_SETMASK: u32 = 2;

/// Bit for `signum` in a signal mask.
pub fn sigmask(signum: u32) -> u32 {
    1 << signum
}

/// Resolve a `kill` signal operand: a name with or without the `SIG` prefix,
/// or a decimal number.
///
/// Signal 0 is accepted because POSIX gives it to `kill` as the
/// "probe whether the process exists" no-op.
pub fn signal_by_name(spec: &str) -> Option<u32> {
    let name = spec.trim();
    let bare = name.strip_prefix("SIG").unwrap_or(name);
    match bare {
        "HUP" => Some(SIGHUP),
        "INT" => Some(SIGINT),
        "KILL" => Some(SIGKILL),
        "PIPE" => Some(SIGPIPE),
        "TERM" => Some(SIGTERM),
        "CHLD" => Some(SIGCHLD),
        "CONT" => Some(SIGCONT),
        "STOP" => Some(SIGSTOP),
        "TSTP" => Some(SIGTSTP),
        _ => name.parse::<u32>().ok().filter(|&n| n < 32),
    }
}

/// Send a signal to one process.
pub fn kill(pid: u64, signal: u32) -> Result<(), Errno> {
    sys::sys_result(unsafe { sys::syscall2(sys::SYS_KILL, pid, signal as u64) }).map(|_| ())
}

/// Send a signal to every process in group `pgid`.
///
/// The kernel reads a negative pid as a group, which is how a terminal aims
/// Ctrl+C at a whole job rather than at one stage of a pipeline.
pub fn kill_group(pgid: u64, signal: u32) -> Result<(), Errno> {
    kill((-(pgid as i64)) as u64, signal)
}

/// Set the disposition for a signal on the calling thread.
///
/// `handler` should be 0 (SIG_DFL) or 1 (SIG_IGN). Answers the previous
/// disposition.
pub fn sys_sigaction(signal: u32, handler: u64) -> Result<u64, Errno> {
    let restorer = if handler > SIG_IGN as u64 {
        sigreturn_trampoline as *const () as usize as u64
    } else {
        0
    };
    sys::sys_result(unsafe { sys::syscall3(sys::SYS_SIGACTION, signal as u64, handler, restorer) })
}

/// Install `handler` as the function to run when `signal` arrives.
///
/// The handler receives the signal number and returns normally; the kernel
/// restores everything the interrupted code was doing, including the return
/// value of a syscall that had already completed.
///
/// Delivery happens when the process next returns from a syscall, so a handler
/// is not a preemption: a process spinning without calling the kernel does not
/// run one. The default action still reaches it, which is why Ctrl+C kills
/// such a process rather than being caught by it.
pub fn signal(signum: u32, handler: extern "C" fn(u32)) -> Result<u64, Errno> {
    sys_sigaction(signum, handler as usize as u64)
}

/// The address a signal handler returns through.
///
/// A handler is entered with this pushed as its return address, so returning
/// lands here and the `sigreturn` below hands the kernel back the frame it
/// saved. These instructions have to live in the process image: the
/// alternative is the kernel writing code onto a stack and making it
/// executable, which is a worse trade than one naked function.
#[unsafe(naked)]
extern "C" fn sigreturn_trampoline() {
    core::arch::naked_asm!(
        "mov rax, {nr}",
        "syscall",
        // sigreturn does not return here; the kernel replaces the whole
        // context. A trap catches the case where it somehow did.
        "ud2",
        nr = const sys::SYS_SIGRETURN,
    )
}

/// Change the calling thread's blocked signal mask.
///
/// `how` is `SIG_BLOCK`, `SIG_UNBLOCK` or `SIG_SETMASK`. Signal sets are 32
/// bits here, so the mask is passed and the previous one returned by value
/// instead of through the `sigset_t` pointers POSIX uses. A blocked signal
/// stays pending until it is unblocked, at which point it is delivered;
/// `SIGKILL` cannot be blocked and is dropped from `mask`. Answers the
/// previous mask; an unknown `how` is an error.
pub fn sigprocmask(how: u32, mask: u32) -> Result<u32, Errno> {
    let ret = unsafe { sys::syscall2(sys::SYS_SIGPROCMASK, how as u64, mask as u64) };
    sys::sys_result(ret).map(|m| m as u32)
}

/// `reboot` commands: what the machine should do once the filesystems are
/// flushed.
pub const REBOOT_POWER_OFF: u64 = 0;
pub const REBOOT_RESTART: u64 = 1;
pub const REBOOT_HALT: u64 = 2;

/// Stop the machine. The kernel syncs every filesystem first, so the next boot
/// does not replay the journal.
///
/// Only returns on an unknown command, and then the reason; a call that is
/// going to work never comes back. A kernel that answered success without
/// stopping is reported as [`Errno::UNKNOWN`], as in [`execve`].
pub fn reboot(cmd: u64) -> Errno {
    let ret = unsafe { sys::syscall1(sys::SYS_REBOOT, cmd) };
    sys::sys_result(ret).err().unwrap_or(Errno::UNKNOWN)
}

/// Syscall number for appointing a shell process.
const SYS_WINDOW_GRANT_SHELL: u64 = 234;

/// Appoint a process as part of the shell, so it may manage windows it does
/// not own: move, resize, frame, minimize, and send focus or close events.
///
/// Only a process that already holds the privilege may grant it, and the
/// kernel seeds exactly one: `bin/edos-init`, the only process it starts.
/// Which programs make up a session is init's policy, so appointing them is
/// init's job rather than a race between whoever claims it first.
///
/// The grant is per pid and is dropped when the process exits, so a later
/// process that reuses the number does not inherit it.
pub fn grant_shell(pid: u64) -> Result<(), Errno> {
    sys::sys_result(unsafe { sys::syscall1(SYS_WINDOW_GRANT_SHELL, pid) }).map(|_| ())
}
