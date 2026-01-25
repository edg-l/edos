//! EDOS Shell - Command-line shell for EDOS GUI terminal.

mod builtins;
mod command;
mod spawn;

use std::io::Write;

// Syscall numbers
const SYS_READ: u64 = 0;
const SYS_POLL: u64 = 7;

/// Poll interest/result state for a file descriptor.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PollState {
    readable: bool,
    writable: bool,
    error: bool,
    hangup: bool,
    invalid: bool,
}

/// A file descriptor to poll with its interests and results.
#[repr(C)]
struct SelectFd {
    fd: u64,
    interests: PollState,
    result: PollState,
}

/// Poll stdin for readability with the given timeout.
/// Returns true if stdin has data available.
fn poll_stdin(timeout_ms: u64) -> bool {
    let mut fds = [SelectFd {
        fd: 0, // stdin
        interests: PollState {
            readable: true,
            ..Default::default()
        },
        result: PollState::default(),
    }];

    let result: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_POLL,
            in("rdi") fds.as_mut_ptr(),
            in("rsi") 1usize,
            in("rdx") timeout_ms,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result > 0 && fds[0].result.readable
}

/// Read from a file descriptor using raw syscall.
/// Returns bytes read, 0 for no data available, or negative for error/EOF.
fn sys_read(fd: u64, buf: &mut [u8]) -> isize {
    let result: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SYS_READ,
            in("rdi") fd,
            in("rsi") buf.as_mut_ptr(),
            in("rdx") buf.len(),
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result as isize
}

/// Read a line from stdin, using poll() to efficiently wait for input.
/// Returns the line (including newline), or None on EOF/error.
///
/// Uses a hybrid approach: poll for efficiency, but always attempt read
/// regardless of poll result to handle race conditions where poll might
/// miss events.
fn read_line() -> Option<String> {
    let mut line = String::new();
    let mut buf = [0u8; 1];

    loop {
        // Try poll first with short timeout for efficiency
        let poll_ready = poll_stdin(100);

        // Always try to read (non-blocking) regardless of poll result.
        // This provides resilience against poll edge cases/races.
        let n = sys_read(0, &mut buf);

        if n < 0 {
            // Error
            return if line.is_empty() { None } else { Some(line) };
        }

        if n == 0 {
            // No data available
            if !poll_ready {
                // Poll timed out and no data - continue polling
                continue;
            }
            // Poll said readable but no data - spurious, retry
            continue;
        }

        let ch = buf[0] as char;

        // Treat both '\r' (carriage return) and '\n' (newline) as line terminators.
        // Terminal sends '\r' for Enter key; normalize to '\n'.
        if ch == '\n' || ch == '\r' {
            line.push('\n');
            return Some(line);
        }

        line.push(ch);
    }
}

fn main() {
    println!("EDOS Shell v0.1");
    println!("Type 'help' for commands.\n");

    let mut stdout = std::io::stdout();

    // Flush welcome message immediately so terminal can display it
    let _ = stdout.flush();

    loop {
        // Print prompt with current directory
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".to_string());
        print!("{} $ ", cwd);

        // Flush stdout (important for piped I/O)
        if stdout.flush().is_err() {
            break;
        }

        // Read line from stdin
        let Some(input) = read_line() else {
            break; // EOF
        };

        // Parse command line into arguments
        let args = command::parse_command(&input);
        if args.is_empty() {
            continue;
        }

        // Split into command and arguments
        let (cmd, rest) = args.split_first().unwrap();

        // Execute the command
        if !command::execute_command(cmd, rest) {
            // execute_command returns false for exit
            break;
        }
    }
}
