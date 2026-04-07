//! Poll and I/O multiplexing utilities.

use crate::sys;

/// Poll interest/result state for a file descriptor.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PollState {
    pub readable: bool,
    pub writable: bool,
    pub error: bool,
    pub hangup: bool,
    pub invalid: bool,
}

/// A file descriptor to poll with its interests and results.
#[repr(C)]
pub struct SelectFd {
    pub fd: u64,
    pub interests: PollState,
    pub result: PollState,
}

/// Poll a set of file descriptors with an optional timeout.
/// Returns the number of ready file descriptors, or a negative error code.
pub fn poll(fds: &mut [SelectFd], timeout_ms: u64) -> i64 {
    unsafe {
        sys::syscall3(
            sys::SYS_POLL,
            fds.as_mut_ptr() as u64,
            fds.len() as u64,
            timeout_ms,
        ) as i64
    }
}

/// Open a file. Returns a file descriptor on success, or negative on error.
/// Flags: 0x40 = O_CREAT, 0x400 = O_APPEND.
pub fn open(path: &str, flags: u64) -> i64 {
    let mut path_buf = std::vec::Vec::with_capacity(path.len() + 1);
    path_buf.extend_from_slice(path.as_bytes());
    path_buf.push(0);
    unsafe { sys::syscall2(sys::SYS_OPEN, path_buf.as_ptr() as u64, flags) as i64 }
}

/// Read from a file descriptor using a raw syscall.
/// Returns bytes read, 0 for no data available, or negative for error/EOF.
pub fn sys_read(fd: u64, buf: &mut [u8]) -> isize {
    unsafe { sys::syscall3(sys::SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) as isize }
}

/// Perform an ioctl on a file descriptor.
/// Returns the ioctl result, or a negative error code.
pub fn ioctl(fd: u64, request: u64, arg: u64) -> i64 {
    unsafe { sys::syscall3(sys::SYS_IOCTL, fd, request, arg) as i64 }
}

/// Map memory. Returns the mapped virtual address, or `!0` on error.
pub fn mmap(addr: u64, length: u64, prot: u64, flags: u64, phys_addr: u64) -> u64 {
    unsafe { sys::syscall5(sys::SYS_MMAP, addr, length, prot, flags, phys_addr) }
}

/// Unmap memory. Returns 0 on success.
pub fn munmap(addr: u64, length: u64) -> i64 {
    unsafe { sys::syscall2(sys::SYS_MUNMAP, addr, length) as i64 }
}

/// Poll stdin for readability with the given timeout.
/// Returns true if stdin has data available.
pub fn poll_stdin(timeout_ms: u64) -> bool {
    let mut fds = [SelectFd {
        fd: 0,
        interests: PollState {
            readable: true,
            ..Default::default()
        },
        result: PollState::default(),
    }];
    let result = poll(&mut fds, timeout_ms);
    result > 0 && fds[0].result.readable
}
