//! Process management, pipe support, and program spawning.

use crate::sys;
use std::ffi::CString;

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
/// Returns the exit status on success, u64::MAX on failure.
pub fn waitpid(pid: u64) -> u64 {
    unsafe { sys::syscall2(sys::SYS_WAIT_PID, pid, 1) }
}

/// A child process with connected I/O pipes.
pub struct ChildProcess {
    /// Process ID
    pub pid: u64,
    /// Write to this to send data to the child's stdin
    pub stdin_write: u64,
    /// Read from this to receive data from the child's stdout
    pub stdout_read: u64,
}

impl ChildProcess {
    /// Spawn a shell and connect pipes.
    ///
    /// # Arguments
    /// * `shell_path` - Path to the shell executable (e.g., "/bin/sh")
    ///
    /// # Returns
    /// A ChildProcess on success, or None on error.
    pub fn spawn_shell(shell_path: &str) -> Option<Self> {
        // Create pipes for stdin and stdout
        let (stdin_read, stdin_write) = pipe()?;
        let (stdout_read, stdout_write) = pipe()?;

        // Spawn the shell
        let pid = spawn(
            shell_path,
            &[],
            stdin_read,   // shell reads from this
            stdout_write, // shell writes to this
            stdout_write, // stderr also goes to stdout
        );

        if pid == u64::MAX {
            // Spawn failed - clean up pipes
            close(stdin_read);
            close(stdin_write);
            close(stdout_read);
            close(stdout_write);
            return None;
        }

        // Close the ends we don't use in the parent
        close(stdin_read);
        close(stdout_write);

        Some(Self {
            pid,
            stdin_write,
            stdout_read,
        })
    }

    /// Write data to the child's stdin.
    pub fn write(&self, data: &[u8]) -> isize {
        write(self.stdin_write, data)
    }

    /// Write a string to the child's stdin.
    pub fn write_str(&self, s: &str) -> isize {
        self.write(s.as_bytes())
    }

    /// Read data from the child's stdout (non-blocking).
    /// Returns the number of bytes read, 0 if no data available, or negative on error.
    pub fn read(&self, buf: &mut [u8]) -> isize {
        read(self.stdout_read, buf)
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
        close(self.stdin_write);
        close(self.stdout_read);
        // Note: we don't wait for the child or kill it here
    }
}

/// Spawn a program with custom fd redirections. Returns Some(pid) on success.
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

    for path in &candidates {
        let Ok(c_path) = CString::new(path.as_str()) else {
            continue;
        };
        let pid = unsafe {
            sys::syscall5(
                sys::SYS_SPAWN,
                c_path.as_ptr() as u64,
                argv_ptrs.as_ptr() as u64,
                stdin_fd,
                stdout_fd,
                stderr_fd,
            )
        };
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
