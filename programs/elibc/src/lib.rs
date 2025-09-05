#![no_std]

//extern crate alloc;

use core::hint::spin_loop;

use crate::syscall::{syscall0, syscall1, syscall2, syscall3, syscall4};

pub mod allocator;
pub mod syscall;

// Syscall numbers
pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_MMAP: u64 = 9;
pub const SYS_MUNMAP: u64 = 11;
pub const SYS_GETPID: u64 = 39;
pub const SYS_EXIT: u64 = 60;
pub const SYS_ERRNO: u64 = 0x400;

// Type-safe wrappers
pub fn sys_write(fd: u64, buf: *const u8, count: usize) -> isize {
    unsafe { syscall3(SYS_WRITE, fd, buf as u64, count as u64) as isize }
}

pub fn sys_read(fd: u64, buf: *mut u8, count: usize) -> isize {
    unsafe { syscall3(SYS_READ, fd, buf as u64, count as u64) as isize }
}

pub fn sys_mmap(addr: u64, length: u64, prot: u32, flags: u32) -> *mut u8 {
    unsafe { syscall4(SYS_MMAP, addr, length, prot as u64, flags as u64) as *mut u8 }
}

pub fn sys_munmap(addr: *mut u8, length: u64) -> i32 {
    unsafe { syscall2(SYS_MUNMAP, addr as u64, length) as i32 }
}

pub fn sys_getpid() -> u64 {
    unsafe { syscall0(SYS_GETPID) }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::upper_case_acronyms)]
pub enum Errno {
    Clear,
    EINVAL,
    ENOMEM,
    EFAULT,
}

pub fn sys_errno() -> u64 {
    unsafe { syscall0(SYS_ERRNO) }
}

pub fn sys_exit(code: i32) -> ! {
    unsafe { syscall1(SYS_EXIT, code as u64) };
    loop {
        spin_loop();
    }
}

// Helper functions
pub fn print(s: &str) {
    sys_write(1, s.as_ptr(), s.len());
}

pub fn println(s: &str) {
    print(s);
    print("\n");
}

// Print formatting support
pub struct Stdout;

impl core::fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        print(s);
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut writer = $crate::Stdout;
        write!(writer, $($arg)*).ok();
    }};
}

#[macro_export]
macro_rules! println {
    () => {
        $crate::print("\n")
    };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print("\n");
    }};
}

// Memory protection flags
pub const PROT_READ: u32 = 0x1;
pub const PROT_WRITE: u32 = 0x2;
pub const PROT_EXEC: u32 = 0x4;

// Memory mapping flags
pub const MAP_PRIVATE: u32 = 0x02;
pub const MAP_ANONYMOUS: u32 = 0x20;

unsafe extern "C" {
    fn main() -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Initialize the heap allocator by triggering first allocation
    //allocator::ALLOCATOR.lock();

    // Call user's main function
    let code = unsafe { main() };

    sys_exit(code);
}

#[panic_handler]
fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    println!("{info}");
    sys_exit(-1);
}
