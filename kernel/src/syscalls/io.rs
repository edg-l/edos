use crate::{println, syscalls::Errno, thread::scheduler::sched};

pub fn sys_write(fd: u64, buffer_ptr: *const u8, count: usize) -> u64 {
    let sched = sched();
    let thread = sched.current_thread_mut();
    thread.errno = Errno::Clear;

    /*
    println!(
        "Syscall: sys_write(fd={}, buf={:p}, count={})",
        fd, buffer_ptr, count
    );
     */

    if count == 0 {
        return 0;
    }

    if fd != 1 {
        thread.errno = Errno::EINVAL;
        return !0u64;
    }

    // Read from user buffer
    let buffer = unsafe { core::slice::from_raw_parts(buffer_ptr, count) };

    // Convert to string (handle invalid UTF-8 gracefully)
    match core::str::from_utf8(buffer) {
        Ok(s) => {
            println!("{}", s); // Use print! instead of println! to not add extra newline
            count as u64 // Return bytes written
        }
        Err(_) => {
            // If not valid UTF-8, print as hex dump
            println!(
                "sys_write: Non-UTF8 data: {:02x?}",
                &buffer[..count.min(64)]
            );
            count as u64
        }
    }
}


pub fn sys_read(fd: u64, buf_ptr: *mut u8, count: u64) -> u64 {
    todo!()
}
