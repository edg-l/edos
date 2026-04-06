//! External program spawning via raw syscalls.

use std::ffi::CString;

// EDOS syscall numbers
const SYS_SPAWN: u64 = 57;
const SYS_WAIT_PID: u64 = 40;
const SYS_PIPE: u64 = 22;
const SYS_CLOSE: u64 = 3;

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

/// Wait for a process to exit (blocking).
/// Returns the PID on success, u64::MAX on failure.
unsafe fn sys_waitpid(pid: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_WAIT_PID,
            in("rdi") pid,
            in("rsi") 1u64, // block = true
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result
}

/// Create a pipe. fds[0] is the read end, fds[1] is the write end.
/// Returns 0 on success, non-zero on failure.
unsafe fn sys_pipe(fds: &mut [u64; 2]) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_PIPE,
            in("rdi") fds.as_mut_ptr(),
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result
}

/// Close a file descriptor.
/// Returns 0 on success, non-zero on failure.
unsafe fn sys_close(fd: u64) -> u64 {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_CLOSE,
            in("rdi") fd,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result
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
            sys_spawn(
                c_path.as_ptr() as *const u8,
                argv_ptrs.as_ptr(),
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
        unsafe {
            sys_waitpid(pid);
        }
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
            let mut fds = [0u64; 2];
            if unsafe { sys_pipe(&mut fds) } != 0 {
                eprintln!("Failed to create pipe");
                // Close any still-open read end from the previous stage
                if let Some(fd) = prev_read_fd {
                    unsafe {
                        sys_close(fd);
                    }
                }
                return;
            }
            (Some(fds[0]), Some(fds[1]))
        } else {
            (None, None)
        };

        let stdin_fd = prev_read_fd.unwrap_or(0);
        let stdout_fd = write_fd.unwrap_or(1);

        let pid = spawn_program_with_fds(cmd, args, stdin_fd, stdout_fd, 2);

        // Close pipe ends the parent no longer needs after spawning
        if let Some(fd) = prev_read_fd {
            unsafe {
                sys_close(fd);
            }
        }
        if let Some(fd) = write_fd {
            unsafe {
                sys_close(fd);
            }
        }

        if pid.is_none() {
            eprintln!("Command not found: {}", cmd);
            // Close the read end of the pipe we just created (if any)
            if let Some(fd) = read_fd {
                unsafe {
                    sys_close(fd);
                }
            }
            return;
        }

        last_pid = pid;
        prev_read_fd = read_fd;
    }

    // Wait for the last process in the pipeline
    if let Some(pid) = last_pid {
        unsafe {
            sys_waitpid(pid);
        }
    }
}
