use core::hint::spin_loop;

use crate::sys::{
    SYS_WAIT_PID,
    calls::{syscall0, syscall1, syscall2, syscall5},
    constants::{SYS_DUP2, SYS_EXIT, SYS_GETPID, SYS_PIPE, SYS_SPAWN},
};

/// Get the process ID
pub fn sys_getpid() -> u64 {
    unsafe { syscall0(SYS_GETPID) }
}

/// Get the process ID
pub fn sys_waitpid(pid: u64, block: bool) -> bool {
    unsafe { syscall2(SYS_WAIT_PID, pid, block as u64) == 1 }
}

/// Exit the process with the given exit code
pub fn sys_exit(code: i32) -> ! {
    unsafe { syscall1(SYS_EXIT, code as u64) };
    loop {
        spin_loop();
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

/// Spawn a new process
/// path: path to executable
/// stdin_fd, stdout_fd, stderr_fd: file descriptors for standard streams (0, 1, 2 for defaults)
/// Returns child PID on success, or u64::MAX on error
pub fn spawn(path: &str, stdin_fd: u64, stdout_fd: u64, stderr_fd: u64) -> u64 {
    let path_ptr = path.as_ptr();
    let argv_ptr = core::ptr::null::<*const u8>(); // TODO: implement argv support
    unsafe {
        syscall5(
            SYS_SPAWN,
            path_ptr as u64,
            argv_ptr as u64,
            stdin_fd,
            stdout_fd,
            stderr_fd,
        )
    }
}

unsafe extern "C" {
    fn main() -> i32;
}

/// Entry point for user programs
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Initialize the heap allocator by triggering first allocation
    crate::allocator::ALLOCATOR.lock();

    // Call user's main function
    let code = unsafe { main() };

    sys_exit(code);
}

/// Panic handler for user programs
#[panic_handler]
pub fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    crate::println!("{info}");
    sys_exit(-1);
}
