//! Process management, pipe support, and program spawning.

use crate::sys;
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

/// Read from a file descriptor.
/// Returns the number of bytes read, or a negative error code.
pub fn read(fd: u64, buf: &mut [u8]) -> isize {
    unsafe { sys::syscall3(sys::SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) as isize }
}

/// Write to a file descriptor.
/// Returns the number of bytes written, or a negative error code.
pub fn write(fd: u64, buf: &[u8]) -> isize {
    unsafe { sys::syscall3(sys::SYS_WRITE, fd, buf.as_ptr() as u64, buf.len() as u64) as isize }
}

/// Duplicate a file descriptor, assigning the lowest unused fd.
/// Returns the new fd, or -1 on error.
pub fn dup(fd: u64) -> i64 {
    unsafe { sys::syscall1(sys::SYS_DUP, fd) as i64 }
}

/// Duplicate a file descriptor to a specific target fd.
/// Returns the new fd, or -1 on error.
pub fn dup2(old_fd: u64, new_fd: u64) -> i64 {
    unsafe { sys::syscall2(sys::SYS_DUP2, old_fd, new_fd) as i64 }
}

/// Get the current process ID.
pub fn getpid() -> u64 {
    unsafe { sys::syscall1(sys::SYS_GETPID, 0) }
}

/// Replace the current process image with `path`, keeping the pid.
///
/// Returns only on failure. Descriptors marked close-on-exec are closed;
/// everything else, along with the cwd and pid, carries into the new image.
pub fn execve(path: &str, args: &[&str], env: &[&str]) -> i64 {
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

    unsafe {
        sys::syscall3(
            sys::SYS_EXECVE,
            path_buf.as_ptr() as u64,
            argv.as_ptr() as u64,
            envp.as_ptr() as u64,
        ) as i64
    }
}

/// `fcntl` commands supported by the kernel.
pub const F_DUPFD: u64 = 0;
pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;
pub const F_DUPFD_CLOEXEC: u64 = 1030;
pub const FD_CLOEXEC: u64 = 1;

/// `fcntl(fd, cmd, arg)`. Returns a negative value on error.
pub fn fcntl(fd: u64, cmd: u64, arg: u64) -> i64 {
    unsafe { sys::syscall3(sys::SYS_FCNTL, fd, cmd, arg) as i64 }
}

/// Mark or unmark a descriptor close-on-exec.
pub fn set_cloexec(fd: u64, on: bool) -> i64 {
    fcntl(fd, F_SETFD, if on { FD_CLOEXEC } else { 0 })
}

/// Real user id of the calling process.
pub fn getuid() -> u32 {
    unsafe { sys::syscall0(sys::SYS_GETUID) as u32 }
}

/// Real group id of the calling process.
pub fn getgid() -> u32 {
    unsafe { sys::syscall0(sys::SYS_GETGID) as u32 }
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
/// Process ID on success, or `u64::MAX` on error.
pub fn spawn(path: &str, args: &[&str], stdin_fd: u64, stdout_fd: u64, stderr_fd: u64) -> u64 {
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

    unsafe {
        sys::syscall5(
            sys::SYS_SPAWN,
            path_buf.as_ptr() as u64,
            argv_ptr as u64,
            stdin_fd,
            stdout_fd,
            stderr_fd,
        )
    }
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
    if ret == u64::MAX { -1 } else { status }
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
    let mut status: i32 = -1;
    let ret = unsafe {
        sys::syscall3(
            sys::SYS_WAIT_PID,
            pid,
            WAIT_UNTRACED,
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
pub fn setpgid(pid: u64, pgid: u64) -> i64 {
    unsafe { sys::syscall2(sys::SYS_SETPGID, pid, pgid) as i64 }
}

/// The process group of `pid`, or of the caller when `pid` is 0.
pub fn getpgid(pid: u64) -> i64 {
    unsafe { sys::syscall1(sys::SYS_GETPGID, pid) as i64 }
}

/// Hand the terminal on `fd` to process group `pgid`.
///
/// The line discipline aims Ctrl+C and Ctrl+Z at whichever group holds the
/// terminal, so this is what "foreground" means.
pub fn tcsetpgrp(fd: u64, pgid: u64) -> i64 {
    unsafe { sys::syscall2(sys::SYS_TCSETPGRP, fd, pgid) as i64 }
}

/// The process group currently holding the terminal on `fd`.
pub fn tcgetpgrp(fd: u64) -> i64 {
    unsafe { sys::syscall1(sys::SYS_TCGETPGRP, fd) as i64 }
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
    /// Spawn a shell connected via a PTY.
    ///
    /// # Arguments
    /// * `shell_path` - Path to the shell executable (e.g., "/bin/sh")
    ///
    /// # Returns
    /// A ChildProcess on success, or None on error.
    pub fn spawn_shell(shell_path: &str) -> Option<Self> {
        let (master_fd, slave_fd) = openpty()?;

        let pid = spawn(
            shell_path,
            &[],
            slave_fd, // shell's stdin
            slave_fd, // shell's stdout
            slave_fd, // shell's stderr
        );

        // Parent closes the slave end regardless of spawn outcome
        close(slave_fd);

        if pid == u64::MAX {
            close(master_fd);
            return None;
        }

        Some(Self { pid, master_fd })
    }

    /// Write data to the child's stdin via the PTY master.
    pub fn write(&self, data: &[u8]) -> isize {
        write(self.master_fd, data)
    }

    /// Write a string to the child's stdin via the PTY master.
    pub fn write_str(&self, s: &str) -> isize {
        self.write(s.as_bytes())
    }

    /// Read data from the child's stdout via the PTY master (non-blocking).
    /// Returns the number of bytes read, 0 if no data available, or negative on error.
    pub fn read(&self, buf: &mut [u8]) -> isize {
        read(self.master_fd, buf)
    }

    /// Try to read available output as a string.
    pub fn read_available(&self) -> Option<String> {
        let mut buf = [0u8; 4096];
        let n = self.read(&mut buf);
        if n > 0 {
            Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
        } else {
            None
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

    // Build envp from current process environment as "KEY=VALUE\0" byte strings
    let mut env_strings: Vec<Vec<u8>> = Vec::new();
    for (key, val) in std::env::vars_os() {
        let key_bytes = key.as_encoded_bytes();
        let val_bytes = val.as_encoded_bytes();
        // Skip entries that contain NUL bytes since they can't be represented as C strings
        if key_bytes.contains(&0) || val_bytes.contains(&0) {
            continue;
        }
        let mut entry = Vec::with_capacity(key_bytes.len() + 1 + val_bytes.len() + 1);
        entry.extend_from_slice(key_bytes);
        entry.push(b'=');
        entry.extend_from_slice(val_bytes);
        entry.push(0); // NUL terminator
        env_strings.push(entry);
    }
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
        if pid != u64::MAX {
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
pub fn spawn_pipeline(stages: &[PipelineStage]) {
    let mut prev_read_fd: Option<u64> = None;
    let mut last_pid: Option<u64> = None;

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
                    return;
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

        if pid.is_none() {
            eprintln!("Command not found: {}", stage.command);
            // Close the read end of the pipe we just created (if any)
            if let Some(fd) = read_fd {
                close(fd);
            }
            return;
        }

        last_pid = pid;
        prev_read_fd = read_fd;
    }

    // Wait for the last process in the pipeline
    if let Some(pid) = last_pid {
        waitpid(pid);
    }
}

/// Fork the calling process (COW).
///
/// Returns the child PID to the parent (positive), 0 to the child,
/// or a negative value on error.
pub fn fork() -> i64 {
    unsafe { sys::syscall0(sys::SYS_FORK) as i64 }
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

/// Send a signal to a process.
///
/// Returns 0 on success, or a negative value on error.
pub fn sys_kill(pid: u64, signal: u32) -> i64 {
    unsafe { sys::syscall2(sys::SYS_KILL, pid, signal as u64) as i64 }
}

/// Send a signal to one process.
pub fn kill(pid: u64, signal: u32) -> i64 {
    sys_kill(pid, signal)
}

/// Send a signal to every process in group `pgid`.
///
/// The kernel reads a negative pid as a group, which is how a terminal aims
/// Ctrl+C at a whole job rather than at one stage of a pipeline.
pub fn kill_group(pgid: u64, signal: u32) -> i64 {
    sys_kill((-(pgid as i64)) as u64, signal)
}

/// Set the disposition for a signal on the calling thread.
///
/// `handler` should be 0 (SIG_DFL) or 1 (SIG_IGN).
/// Returns the previous disposition on success, or a negative value on error.
pub fn sys_sigaction(signal: u32, handler: u64) -> i64 {
    let restorer = if handler > SIG_IGN as u64 {
        sigreturn_trampoline as *const () as usize as u64
    } else {
        0
    };
    unsafe { sys::syscall3(sys::SYS_SIGACTION, signal as u64, handler, restorer) as i64 }
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
pub fn signal(signum: u32, handler: extern "C" fn(u32)) -> i64 {
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
/// `SIGKILL` cannot be blocked and is dropped from `mask`.
///
/// Returns the previous mask, or -1 on an unknown `how`.
pub fn sigprocmask(how: u32, mask: u32) -> i64 {
    unsafe { sys::syscall2(sys::SYS_SIGPROCMASK, how as u64, mask as u64) as i64 }
}

/// `reboot` commands: what the machine should do once the filesystems are
/// flushed.
pub const REBOOT_POWER_OFF: u64 = 0;
pub const REBOOT_RESTART: u64 = 1;
pub const REBOOT_HALT: u64 = 2;

/// Stop the machine. The kernel syncs every filesystem first, so the next boot
/// does not replay the journal.
///
/// Only returns on an unknown command, and then -1; a call that is going to
/// work never comes back.
pub fn reboot(cmd: u64) -> i64 {
    unsafe { sys::syscall1(sys::SYS_REBOOT, cmd) as i64 }
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
pub fn grant_shell(pid: u64) -> Result<(), i64> {
    let ret = unsafe { crate::sys::syscall1(SYS_WINDOW_GRANT_SHELL, pid) };
    if ret == u64::MAX {
        return Err(-1);
    }
    Ok(())
}
