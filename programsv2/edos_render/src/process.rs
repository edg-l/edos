//! Process management and pipe support for terminal applications.

use std::arch::asm;

// Syscall numbers
const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_CLOSE: u64 = 3;
const SYS_PIPE: u64 = 22;
const SYS_SPAWN: u64 = 57;

/// Raw syscall with 1 argument.
#[inline(always)]
unsafe fn syscall1(num: u64, arg1: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 2 arguments.
#[inline(always)]
#[allow(dead_code)]
unsafe fn syscall2(num: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 3 arguments.
#[inline(always)]
unsafe fn syscall3(num: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 5 arguments.
#[inline(always)]
unsafe fn syscall5(num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Create a pipe for inter-process communication.
/// Returns (read_fd, write_fd) on success, or None on error.
pub fn pipe() -> Option<(u64, u64)> {
    let mut pipefd = [0u64; 2];
    let result = unsafe { syscall1(SYS_PIPE, pipefd.as_mut_ptr() as u64) };
    if result == 0 {
        Some((pipefd[0], pipefd[1]))
    } else {
        None
    }
}

/// Close a file descriptor.
pub fn close(fd: u64) -> i32 {
    unsafe { syscall1(SYS_CLOSE, fd) as i32 }
}

/// Read from a file descriptor.
/// Returns the number of bytes read, or a negative error code.
pub fn read(fd: u64, buf: &mut [u8]) -> isize {
    unsafe { syscall3(SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) as isize }
}

/// Write to a file descriptor.
/// Returns the number of bytes written, or a negative error code.
pub fn write(fd: u64, buf: &[u8]) -> isize {
    unsafe { syscall3(SYS_WRITE, fd, buf.as_ptr() as u64, buf.len() as u64) as isize }
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
        syscall5(
            SYS_SPAWN,
            path_buf.as_ptr() as u64,
            argv_ptr as u64,
            stdin_fd,
            stdout_fd,
            stderr_fd,
        )
    }
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
