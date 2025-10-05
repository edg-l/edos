use alloc::vec::Vec;
use core::hint::spin_loop;

use crate::sys::{
    Errno, SYS_WAIT_PID,
    calls::{syscall0, syscall1, syscall2, syscall3, syscall4, syscall5},
    constants::{
        SYS_CLONE, SYS_DUP, SYS_DUP2, SYS_EXIT, SYS_GETPID, SYS_MONOTONIC_TIME, SYS_PIPE,
        SYS_SLEEP_MS, SYS_SPAWN,
    },
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

/// Retrieve monotonic time since boot in nanoseconds.
pub fn monotonic_time_ns() -> Result<u64, Errno> {
    let result = unsafe { syscall0(SYS_MONOTONIC_TIME) };
    if result == u64::MAX {
        Err(errno())
    } else {
        Ok(result)
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

/// Duplicate file descriptor to the lowest available descriptor number
/// Returns the new descriptor on success, or `u64::MAX` on error
pub fn dup(oldfd: u64) -> u64 {
    unsafe { syscall1(SYS_DUP, oldfd) }
}

/// Clone the calling thread to create a new thread.
///
/// * `func`: function pointer to execute in the new thread
/// * `arg`: argument to pass to the function
/// * `flags`: clone flags (currently unused)
/// * `stack`: user stack pointer for the child thread (0 = kernel allocates)
///
/// Returns child thread ID on success, or `u64::MAX` on error.
pub fn sys_clone(
    func: extern "C" fn(*mut u8) -> i32,
    arg: *mut u8,
    flags: u64,
    stack: *mut u8,
) -> u64 {
    unsafe { syscall4(SYS_CLONE, func as u64, arg as u64, flags, stack as u64) }
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

// Thread support
pub type ThreadId = u64;

/// Thread creation result
pub type ThreadResult = Result<ThreadId, Errno>;

/// Internal structure to pass both function and argument to thread wrapper
#[repr(C)]
struct ThreadStartData {
    func: extern "C" fn(*mut u8) -> i32,
    arg: *mut u8,
}

/// Hidden thread entry wrapper that ensures threads exit cleanly when they return.
/// This is analogous to _start for main().
extern "C" fn thread_entry_wrapper(data_ptr: *mut u8) -> i32 {
    // Unpack the thread start data
    let data = unsafe { &*(data_ptr as *const ThreadStartData) };
    let func = data.func;
    let arg = data.arg;

    // Deallocate the ThreadStartData box
    unsafe {
        let _ = alloc::boxed::Box::from_raw(data_ptr as *mut ThreadStartData);
    }

    // Call the user's thread function
    let exit_code = func(arg);

    // Exit the thread cleanly
    sys_exit(exit_code);
}

/// Create a new thread that executes the given function.
///
/// * `func`: function to execute in the new thread
/// * `arg`: argument to pass to the function
///
/// Returns the thread ID on success.
pub fn thread_create(func: extern "C" fn(*mut u8) -> i32, arg: *mut u8) -> ThreadResult {
    // Pack function and argument into a box
    let data = alloc::boxed::Box::new(ThreadStartData { func, arg });
    let data_ptr = alloc::boxed::Box::into_raw(data) as *mut u8;

    // Create thread with wrapper function
    let tid = sys_clone(thread_entry_wrapper, data_ptr, 0, core::ptr::null_mut());
    if tid == u64::MAX {
        // If clone failed, deallocate the box
        unsafe {
            let _ = alloc::boxed::Box::from_raw(data_ptr as *mut ThreadStartData);
        }
        Err(errno())
    } else {
        Ok(tid)
    }
}

/// Wait for a thread to complete and retrieve its exit status.
///
/// * `tid`: thread ID to wait for
///
/// Returns the thread's exit code on success.
pub fn thread_join(tid: ThreadId) -> Result<i32, Errno> {
    loop {
        match sys_waitpid(tid, false)? {
            WaitPidStatus::Exited(code) => return Ok(code),
            WaitPidStatus::StillRunning => {
                // Sleep a bit to avoid busy waiting
                let _ = sleep_ms(1);
            }
        }
    }
}
