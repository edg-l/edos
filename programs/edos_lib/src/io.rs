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

/// Returns true if the file descriptor refers to a terminal.
pub fn isatty(fd: u64) -> bool {
    unsafe { sys::syscall1(sys::SYS_ISATTY, fd) == 1 }
}

/// Open a file. Returns a file descriptor on success, or negative on error.
/// Flags: 0x40 = O_CREAT, 0x400 = O_APPEND.
pub fn open(path: &str, flags: u64) -> i64 {
    let mut path_buf = std::vec::Vec::with_capacity(path.len() + 1);
    path_buf.extend_from_slice(path.as_bytes());
    path_buf.push(0);
    unsafe { sys::syscall2(sys::SYS_OPEN, path_buf.as_ptr() as u64, flags) as i64 }
}

/// One directory entry as the kernel writes it, immediately followed in the
/// buffer by `name_len` bytes of name.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    pub name_len: u32,
    /// 0=file, 1=directory, 2=symlink, 3=special, 4=device.
    pub file_type: u8,
    pub size: u64,
    /// readonly=1, hidden=2, system=4, archive=8.
    pub attrs: u8,
    pub reserved: [u8; 2],
}

/// Read the entries of `path` into `buf`, starting at entry index `start`.
///
/// Returns the number of bytes written, or 0 once `start` is past the last
/// entry. A directory larger than `buf` is enumerated by calling again with
/// `start` advanced by the number of entries decoded; -1 with `EINVAL` means
/// `buf` is too small to hold even the entry at `start`.
pub fn getdents(path: &str, buf: &mut [u8], start: usize) -> i64 {
    unsafe {
        sys::syscall5(
            sys::SYS_GETDENTS,
            path.as_ptr() as u64,
            path.len() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            start as u64,
        ) as i64
    }
}

/// Decode the first `len` bytes of a buffer filled by [`getdents`] into entries
/// paired with their names. Stops at the first truncated record.
pub fn parse_dents(buf: &[u8], len: usize) -> std::vec::Vec<(DirEntry, std::string::String)> {
    let header = core::mem::size_of::<DirEntry>();
    let mut out = std::vec::Vec::new();
    let mut offset = 0usize;

    while offset + header <= len {
        let entry = unsafe { core::ptr::read_unaligned(buf[offset..].as_ptr() as *const DirEntry) };
        let name_start = offset + header;
        let name_end = name_start + entry.name_len as usize;
        if name_end > len {
            break;
        }
        out.push((
            entry,
            std::string::String::from_utf8_lossy(&buf[name_start..name_end]).into_owned(),
        ));
        offset = name_end;
    }

    out
}

/// Mode bits for [`access`], as in POSIX `<unistd.h>`.
pub const F_OK: u32 = 0;
pub const X_OK: u32 = 1;
pub const W_OK: u32 = 2;
pub const R_OK: u32 = 4;

/// Test a path against an access mode. Returns 0 if the access is permitted
/// and -1 otherwise; `F_OK` is an existence test.
///
/// EDOS has no per-file permission bits and every process runs with the same
/// credentials, so the answer is existence plus the read-only attribute:
/// `W_OK` on a read-only file is denied, `R_OK` and `X_OK` are granted for
/// anything that exists.
pub fn access(path: &str, mode: u32) -> i64 {
    unsafe {
        sys::syscall3(
            sys::SYS_ACCESS,
            path.as_ptr() as u64,
            path.len() as u64,
            mode as u64,
        ) as i64
    }
}

/// Resize the file named by `path` to `size` bytes, extending it with zeros
/// when it grows. Returns 0 on success and -1 on error; a directory is
/// refused.
pub fn truncate(path: &str, size: u64) -> i64 {
    unsafe {
        sys::syscall3(
            sys::SYS_TRUNCATE,
            path.as_ptr() as u64,
            path.len() as u64,
            size,
        ) as i64
    }
}

/// POSIX `struct timespec`, as passed to [`utimensat`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// `tv_nsec` values that name a time instead of carrying one: take the current
/// time, or leave the timestamp alone.
pub const UTIME_NOW: i64 = (1 << 30) - 1;
pub const UTIME_OMIT: i64 = (1 << 30) - 2;

/// `dirfd` naming the calling process's working directory.
pub const AT_FDCWD: i64 = -100;

/// Set the access and modification times of `path`: `times[0]` is the access
/// time and `times[1]` the modification time, and `None` stamps both with the
/// current time. Returns 0 on success and -1 on error.
///
/// A relative path resolves against the directory descriptor `dirfd`, or the
/// working directory for [`AT_FDCWD`]. Timestamps are stored to whole seconds,
/// and `flags` must be 0.
pub fn utimensat(dirfd: i64, path: &str, times: Option<&[Timespec; 2]>, flags: u64) -> i64 {
    let times_ptr = times.map(|t| t.as_ptr() as u64).unwrap_or(0);
    unsafe {
        sys::syscall5(
            sys::SYS_UTIMENSAT,
            dirfd as u64,
            path.as_ptr() as u64,
            path.len() as u64,
            times_ptr,
            flags,
        ) as i64
    }
}

/// Set both timestamps of `path` to whole-second Unix times.
pub fn set_file_times(path: &str, atime_secs: i64, mtime_secs: i64) -> i64 {
    let times = [
        Timespec {
            tv_sec: atime_secs,
            tv_nsec: 0,
        },
        Timespec {
            tv_sec: mtime_secs,
            tv_nsec: 0,
        },
    ];
    utimensat(AT_FDCWD, path, Some(&times), 0)
}

/// Create a symbolic link at `path` holding `target`. The target is stored
/// verbatim: a relative one resolves against the link's own directory, and a
/// link to a path that does not exist is legal. Returns 0 on success and -1 on
/// error.
pub fn symlink(target: &str, path: &str) -> i64 {
    unsafe {
        sys::syscall4(
            sys::SYS_SYMLINK,
            target.as_ptr() as u64,
            target.len() as u64,
            path.as_ptr() as u64,
            path.len() as u64,
        ) as i64
    }
}

/// Read the target of the symbolic link at `path` into `buf`, without a
/// terminating NUL. Returns the number of bytes written, which equals
/// `buf.len()` when the target was truncated, or -1 on error.
pub fn readlink(path: &str, buf: &mut [u8]) -> i64 {
    unsafe {
        sys::syscall4(
            sys::SYS_READLINK,
            path.as_ptr() as u64,
            path.len() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as i64
    }
}

/// What `stat` reports about a file.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Stat {
    pub size: u64,
    /// Creation, access and modification times, in whole Unix seconds.
    pub created: u64,
    pub accessed: u64,
    pub modified: u64,
    pub attrs: u16,
    pub kind: u8,
}

/// Stat a path. Returns `None` when it cannot be resolved.
pub fn stat(path: &str) -> Option<Stat> {
    let mut out = Stat::default();
    let rc = unsafe {
        sys::syscall3(
            sys::SYS_STAT,
            path.as_ptr() as u64,
            path.len() as u64,
            &mut out as *mut Stat as u64,
        ) as i64
    };
    if rc == 0 { Some(out) } else { None }
}

/// Stat a path relative to the directory descriptor `dirfd`. An absolute path
/// ignores `dirfd`, and [`AT_FDCWD`] names the working directory. `flags` must
/// be 0: the walk always follows symbolic links, so `AT_SYMLINK_NOFOLLOW` is
/// refused rather than quietly ignored.
pub fn fstatat(dirfd: i64, path: &str, flags: u64) -> Option<Stat> {
    let mut out = Stat::default();
    let rc = unsafe {
        sys::syscall5(
            sys::SYS_FSTATAT,
            dirfd as u64,
            path.as_ptr() as u64,
            path.len() as u64,
            &mut out as *mut Stat as u64,
            flags,
        ) as i64
    };
    if rc == 0 { Some(out) } else { None }
}

/// Open a file relative to the directory descriptor `dirfd`. An absolute path
/// ignores `dirfd`, and [`AT_FDCWD`] names the working directory. `flags` are
/// [`open`]'s. Returns a file descriptor, or negative on error.
pub fn openat(dirfd: i64, path: &str, flags: u64) -> i64 {
    unsafe {
        sys::syscall4(
            sys::SYS_OPENAT,
            dirfd as u64,
            path.as_ptr() as u64,
            path.len() as u64,
            flags,
        ) as i64
    }
}

/// Create a directory relative to the directory descriptor `dirfd`. Returns 0
/// on success and -1 on error.
pub fn mkdirat(dirfd: i64, path: &str) -> i64 {
    unsafe {
        sys::syscall3(
            sys::SYS_MKDIRAT,
            dirfd as u64,
            path.as_ptr() as u64,
            path.len() as u64,
        ) as i64
    }
}

/// `flags` bit making [`unlinkat`] remove an empty directory instead of a file.
pub const AT_REMOVEDIR: u64 = 0x200;

/// Remove a file, or with [`AT_REMOVEDIR`] an empty directory, relative to the
/// directory descriptor `dirfd`. Returns 0 on success and -1 on error.
pub fn unlinkat(dirfd: i64, path: &str, flags: u64) -> i64 {
    unsafe {
        sys::syscall4(
            sys::SYS_UNLINKAT,
            dirfd as u64,
            path.as_ptr() as u64,
            path.len() as u64,
            flags,
        ) as i64
    }
}

/// Read from a file descriptor using a raw syscall.
/// Returns bytes read, 0 for no data available, or negative for error/EOF.
pub fn sys_read(fd: u64, buf: &mut [u8]) -> isize {
    unsafe { sys::syscall3(sys::SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) as isize }
}

/// Read at an explicit offset without moving the descriptor's own offset.
///
/// Unlike `lseek` + `read`, this is safe to call from several threads sharing
/// one descriptor: the offset is an argument rather than shared state. Only
/// regular files support it; pipes, sockets and terminals return ESPIPE.
pub fn pread(fd: u64, buf: &mut [u8], offset: u64) -> isize {
    unsafe {
        sys::syscall4(
            sys::SYS_PREAD,
            fd,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            offset,
        ) as isize
    }
}

/// Write at an explicit offset without moving the descriptor's own offset.
pub fn pwrite(fd: u64, buf: &[u8], offset: u64) -> isize {
    unsafe {
        sys::syscall4(
            sys::SYS_PWRITE,
            fd,
            buf.as_ptr() as u64,
            buf.len() as u64,
            offset,
        ) as isize
    }
}

/// Perform an ioctl on a file descriptor.
/// Returns the ioctl result, or a negative error code.
pub fn ioctl(fd: u64, request: u64, arg: u64) -> i64 {
    unsafe { sys::syscall3(sys::SYS_IOCTL, fd, request, arg) as i64 }
}

pub const PTY_IOCTL_SET_RAW: u64 = 0x5001;
pub const PTY_IOCTL_SET_CANONICAL: u64 = 0x5002;
pub const PTY_IOCTL_GET_MODE: u64 = 0x5003;

/// Switch the PTY slave fd to raw mode (no echo, no line buffering).
pub fn pty_set_raw(fd: u64) {
    ioctl(fd, PTY_IOCTL_SET_RAW, 0);
}

/// Switch the PTY slave fd to canonical mode (echo enabled, line buffering).
pub fn pty_set_canonical(fd: u64) {
    ioctl(fd, PTY_IOCTL_SET_CANONICAL, 0);
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
