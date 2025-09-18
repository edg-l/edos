use crate::sys::{calls::syscall0, constants::SYS_ERRNO};

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::upper_case_acronyms)]
#[repr(u64)]
pub enum Errno {
    /// No error; used to clear the errno field.
    Clear,
    /// Invalid argument passed to a syscall.
    EINVAL,
    /// Memory allocation failed or memory exhausted.
    ENOMEM,
    /// Bad memory address provided by userspace.
    EFAULT,
    /// Invalid or closed file descriptor.
    EBADF,
    /// Operation requires permissions the caller lacks.
    EACCES,
    /// Operation not permitted for the current caller.
    EPERM,
    /// Requested file or directory does not exist.
    ENOENT,
    /// Attempted to create an entry that already exists.
    EEXIST,
    /// Expected a directory but encountered a non-directory entry.
    ENOTDIR,
    /// Operation required a regular file but encountered a directory.
    EISDIR,
    /// Device or filesystem has no space left for the operation.
    ENOSPC,
    /// Write attempted on a read-only filesystem or device.
    EROFS,
    /// Generic I/O failure surfaced from the filesystem or storage layer.
    EIO,
    /// Placeholder for unknown or unmapped kernel error codes.
    UNKNOWN,
}

pub fn errno() -> Errno {
    unsafe { core::mem::transmute(syscall0(SYS_ERRNO)) }
}
