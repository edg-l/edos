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
    let ret = unsafe { sys::syscall3(sys::SYS_WAIT_PID, pid, 1, &mut status as *mut i32 as u64) };
    if ret == u64::MAX { -1 } else { status }
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

/// Spawn a pipeline of commands connected by pipes.
/// Each stage is (command_name, args_vec).
pub fn spawn_pipeline(stages: &[(String, Vec<String>)]) {
    if stages.len() == 1 {
        spawn_program(&stages[0].0, &stages[0].1);
        return;
    }

    let mut prev_read_fd: Option<u64> = None;
    let mut last_pid: Option<u64> = None;

    for (i, (cmd, args)) in stages.iter().enumerate() {
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

        let stdin_fd = prev_read_fd.unwrap_or(0);
        let stdout_fd = write_fd.unwrap_or(1);

        let pid = spawn_program_with_fds(cmd, args, stdin_fd, stdout_fd, 2);

        // Close pipe ends the parent no longer needs after spawning
        if let Some(fd) = prev_read_fd {
            close(fd);
        }
        if let Some(fd) = write_fd {
            close(fd);
        }

        if pid.is_none() {
            eprintln!("Command not found: {}", cmd);
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
