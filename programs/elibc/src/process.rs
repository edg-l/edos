use core::hint::spin_loop;

use crate::sys::{
    calls::syscall0,
    calls::syscall1,
    constants::{SYS_EXIT, SYS_GETPID},
};

/// Get the process ID
pub fn sys_getpid() -> u64 {
    unsafe { syscall0(SYS_GETPID) }
}

/// Exit the process with the given exit code
pub fn sys_exit(code: i32) -> ! {
    unsafe { syscall1(SYS_EXIT, code as u64) };
    loop {
        spin_loop();
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
