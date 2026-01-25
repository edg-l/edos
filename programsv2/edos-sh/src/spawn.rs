//! External program spawning via raw syscalls.

use std::ffi::CString;

// EDOS syscall numbers
const SYS_SPAWN: u64 = 57;
const SYS_WAIT_PID: u64 = 40;

/// Spawn a process using the EDOS spawn syscall.
/// Returns the PID on success, u64::MAX on failure.
unsafe fn sys_spawn(
    path: *const u8,
    argv: *const *const u8,
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_SPAWN,
            in("rdi") path,
            in("rsi") argv,
            in("rdx") stdin_fd,
            in("r10") stdout_fd,
            in("r8") stderr_fd,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result
}

/// Wait for a process to exit.
/// Returns the exit code on success, u64::MAX on failure.
unsafe fn sys_waitpid(pid: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_WAIT_PID,
            in("rdi") pid,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result
}

/// Try to spawn an external program.
pub fn spawn_program(command: &str, args: &[String]) {
    // Build candidate paths to search
    let candidates = [
        format!("/bin/{}", command),
        format!("./{}", command),
        format!("/usr/bin/{}", command),
        format!("/{}", command),
    ];

    // Build argv as C strings
    let mut c_args: Vec<CString> = Vec::with_capacity(args.len() + 1);
    c_args.push(CString::new(command).unwrap_or_default());
    for arg in args {
        if let Ok(c) = CString::new(arg.as_str()) {
            c_args.push(c);
        }
    }

    // Build argv pointer array (null-terminated)
    let mut argv_ptrs: Vec<*const u8> = c_args.iter().map(|c| c.as_ptr() as *const u8).collect();
    argv_ptrs.push(std::ptr::null());

    // Try each candidate path
    for path in &candidates {
        let Ok(c_path) = CString::new(path.as_str()) else {
            continue;
        };

        let pid = unsafe {
            sys_spawn(
                c_path.as_ptr() as *const u8,
                argv_ptrs.as_ptr(),
                0, // stdin
                1, // stdout
                2, // stderr
            )
        };

        if pid != u64::MAX {
            // Wait for the process to complete
            unsafe {
                sys_waitpid(pid);
            }
            return;
        }
    }

    // Command not found
    eprintln!("Command not found: {}", command);
}
