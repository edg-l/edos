use alloc::vec::Vec;
use core::hint::spin_loop;

use crate::sys::{
    Errno, SYS_WAIT_PID,
    calls::{syscall0, syscall1, syscall2, syscall3, syscall5},
    constants::{SYS_DUP2, SYS_EXIT, SYS_GETPID, SYS_PIPE, SYS_SLEEP_MS, SYS_SPAWN},
    errno,
};

/// Get the process ID
pub fn sys_getpid() -> u64 {
    unsafe { syscall0(SYS_GETPID) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPidStatus {
    StillRunning,
    Exited(i32),
}

/// Wait for a child process and optionally retrieve its exit status.
///
/// Returns [`WaitPidStatus::StillRunning`] when the process is still alive.
/// Returns [`WaitPidStatus::Exited`] with the child's exit code when it has terminated.
/// Returns [`Err`] with the current [`Errno`] when the syscall fails.
pub fn sys_waitpid(pid: u64, block: bool) -> Result<WaitPidStatus, Errno> {
    let mut status = 0i32;
    let result = unsafe {
        syscall3(
            SYS_WAIT_PID,
            pid,
            block as u64,
            (&mut status as *mut i32) as u64,
        )
    };

    if result == u64::MAX {
        return Err(errno());
    }

    if result == 0 {
        Ok(WaitPidStatus::StillRunning)
    } else {
        Ok(WaitPidStatus::Exited(status))
    }
}

/// Exit the process with the given exit code
pub fn sys_exit(code: i32) -> ! {
    unsafe { syscall1(SYS_EXIT, code as u64) };
    loop {
        spin_loop();
    }
}

/// Sleep the current thread for at least `ms` milliseconds.
pub fn sleep_ms(ms: u64) -> Result<(), Errno> {
    let result = unsafe { syscall1(SYS_SLEEP_MS, ms) };
    if result == u64::MAX {
        Err(errno())
    } else {
        Ok(())
    }
}

/// Create a pipe for inter-process communication
/// Returns (read_fd, write_fd) on success, or None on error
pub fn pipe() -> Option<(u64, u64)> {
    let mut pipefd = [0u64; 2];
    let result = unsafe { syscall1(SYS_PIPE, pipefd.as_mut_ptr() as u64) };
    if result == 0 {
        Some((pipefd[0], pipefd[1]))
    } else {
        None
    }
}

/// Duplicate file descriptor oldfd to newfd
/// Returns newfd on success, or u64::MAX on error
pub fn dup2(oldfd: u64, newfd: u64) -> u64 {
    unsafe { syscall2(SYS_DUP2, oldfd, newfd) }
}

/// Spawn a new process.
///
/// * `path`: path to executable (must be a valid UTF-8 string)
/// * `args`: argument vector (each argument will be converted to a C string)
/// * `stdin_fd`, `stdout_fd`, `stderr_fd`: file descriptors for standard streams (0, 1, 2 for defaults)
///
/// Returns child PID on success, or `u64::MAX` on error.
pub fn spawn(path: &str, args: &[&str], stdin_fd: u64, stdout_fd: u64, stderr_fd: u64) -> u64 {
    let mut path_buf = Vec::with_capacity(path.len() + 1);
    path_buf.extend_from_slice(path.as_bytes());
    path_buf.push(0);

    let mut argv_storage: Vec<Vec<u8>> = Vec::with_capacity(args.len());
    let mut argv_ptrs: Vec<*const u8> = Vec::with_capacity(args.len() + 1);

    for &arg in args {
        let mut buf = Vec::with_capacity(arg.len() + 1);
        buf.extend_from_slice(arg.as_bytes());
        buf.push(0);
        argv_ptrs.push(buf.as_ptr());
        argv_storage.push(buf);
    }

    // Null-terminate the argv list following C conventions.
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

unsafe extern "C" {
    fn main(argc: isize, argv: *const *const u8) -> i32;
}

/// Entry point for user programs
///
/// # Safety
/// Pointers must be valid
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start(argc: isize, argv: *const *const u8) -> ! {
    // Initialize the heap allocator by triggering first allocation
    crate::allocator::ALLOCATOR.lock();

    // Call user's main function
    let code = unsafe { main(argc, argv) };

    sys_exit(code);
}

/// Panic handler for user programs
#[panic_handler]
pub fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    crate::println!("{info}");
    sys_exit(-1);
}
