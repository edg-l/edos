//! Memory mapping syscall wrappers.

use crate::sys;

// Memory protection flags
pub const PROT_READ: u32 = 0x1;
pub const PROT_WRITE: u32 = 0x2;
pub const PROT_EXEC: u32 = 0x4;

// Memory mapping flags
pub const MAP_SHARED: u32 = 0x01;
pub const MAP_PRIVATE: u32 = 0x02;
pub const MAP_FIXED: u32 = 0x10;
pub const MAP_ANONYMOUS: u32 = 0x20;

// msync flags
pub const MS_ASYNC: u32 = 0x1;
pub const MS_SYNC: u32 = 0x2;
pub const MS_INVALIDATE: u32 = 0x4;

/// Map memory into the address space.
///
/// For anonymous mappings pass `fd = -1` and `file_offset = 0`.
/// For file-backed mappings pass the open file descriptor and the byte offset
/// (must be 4 KiB aligned).
pub fn mmap(
    addr: *mut u8,
    length: u64,
    prot: u32,
    flags: u32,
    fd: i32,
    file_offset: u64,
) -> *mut u8 {
    let ret = unsafe {
        sys::syscall6(
            sys::SYS_MMAP,
            addr as u64,
            length,
            prot as u64,
            flags as u64,
            fd as u64,
            file_offset,
        )
    };
    // A negated errno is a plausible-looking address, so it is collapsed to the
    // `!0` this function has always reported; `errno` still carries the code.
    if sys::is_err(ret) {
        u64::MAX as *mut u8
    } else {
        ret as *mut u8
    }
}

/// Unmap memory from the address space.
pub fn munmap(addr: *mut u8, length: u64) -> i32 {
    unsafe { sys::syscall2(sys::SYS_MUNMAP, addr as u64, length) as i32 }
}

/// Change the protection of pages already mapped.
///
/// `addr` must be page-aligned and the whole range must be mapped; a hole
/// anywhere in it leaves every page as it was and reports `ENOMEM`.
pub fn mprotect(addr: *mut u8, length: u64, prot: u32) -> i32 {
    unsafe { sys::syscall3(sys::SYS_MPROTECT, addr as u64, length, prot as u64) as i32 }
}

/// Flush or invalidate a range of memory-mapped file pages.
///
/// `flags` should be one of `MS_ASYNC`, `MS_SYNC`, or `MS_INVALIDATE`.
///
/// # Safety
/// `addr` must point to a page-aligned address within a file-backed mapping of
/// at least `length` bytes.
pub unsafe fn msync(addr: *mut u8, length: u64, flags: u32) -> i32 {
    unsafe { sys::syscall3(sys::SYS_MSYNC, addr as u64, length, flags as u64) as i32 }
}
