#![no_std]
#![expect(clippy::too_many_arguments)]

extern crate alloc;

// Core modules
pub mod allocator;
pub mod graphics;
pub mod io;
pub mod math;
pub mod memory;
pub mod process;
pub mod sys;

// Re-export commonly used types and functions for convenience
pub use memory::{mmap, munmap};
pub use process::{sys_exit, sys_getpid};
pub use sys::{Errno, errno};

// File I/O syscalls
use crate::sys::{calls::syscall1, calls::syscall3, constants::*};

/// # Safety
/// Caller must ensure:
/// - `fd` is a valid file descriptor
/// - `buf` points to readable memory of at least `count` bytes
/// - `buf` remains valid for the duration of the syscall
pub unsafe fn sys_write(fd: u64, buf: *const u8, count: usize) -> isize {
    unsafe { syscall3(SYS_WRITE, fd, buf as u64, count as u64) as isize }
}

/// # Safety
/// Caller must ensure:
/// - `fd` is a valid file descriptor
/// - `buf` points to writable memory of at least `count` bytes
/// - `buf` remains valid for the duration of the syscall
pub unsafe fn sys_read(fd: u64, buf: *mut u8, count: usize) -> isize {
    unsafe { syscall3(SYS_READ, fd, buf as u64, count as u64) as isize }
}

pub fn sys_close(fd: u64) -> i32 {
    unsafe { syscall1(SYS_CLOSE, fd) as i32 }
}

// Re-export I/O types for convenience
pub use io::{
    IoError, IoResult, KeyEvent, STDERR, STDOUT, get_raw_input, read_from_fd, read_stdin,
};
// Re-export memory constants
pub use sys::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_EXEC, PROT_READ, PROT_WRITE};

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        let _ = $crate::STDOUT.lock().write_fmt(format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print!("\n");
    }};
}

#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {{
        let _ = $crate::STDERR.lock().write_fmt(format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! eprintln {
    () => {
        $crate::eprint!("\n")
    };
    ($($arg:tt)*) => {{
        $crate::eprint!($($arg)*);
        $crate::eprint!("\n");
    }};
}

// Process entry point and panic handler are now in process.rs
pub use process::{_start, rust_panic};
