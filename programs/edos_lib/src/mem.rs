//! Memory mapping syscall wrappers.

use crate::sys::{self, Errno};
use core::ptr::NonNull;

// Memory protection flags
pub const PROT_READ: u32 = 0x1;
pub const PROT_WRITE: u32 = 0x2;
pub const PROT_EXEC: u32 = 0x4;

// Memory mapping flags
pub const MAP_SHARED: u32 = 0x01;
pub const MAP_PRIVATE: u32 = 0x02;
pub const MAP_FIXED: u32 = 0x10;
pub const MAP_ANONYMOUS: u32 = 0x20;
pub const MAP_PHYSICAL: u32 = 0x40;
pub const MAP_WRITE_COMBINING: u32 = 0x80;

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
) -> Result<NonNull<u8>, Errno> {
    map(addr as u64, length, prot, flags, fd as u64, file_offset)
}

/// Map a range of physical memory, such as a framebuffer's VRAM aperture.
///
/// The kernel overloads the fifth argument: with `MAP_PHYSICAL` it is the
/// physical base rather than a descriptor, so the two forms are separate
/// entry points instead of one call whose meaning depends on a flag bit.
pub fn mmap_physical(
    length: u64,
    prot: u32,
    flags: u32,
    phys_addr: u64,
) -> Result<NonNull<u8>, Errno> {
    map(0, length, prot, flags | MAP_PHYSICAL, phys_addr, 0)
}

fn map(
    addr: u64,
    length: u64,
    prot: u32,
    flags: u32,
    r8: u64,
    r9: u64,
) -> Result<NonNull<u8>, Errno> {
    // A negated errno is a plausible-looking address once it is in a `u64`, and
    // so is a null one; `NonNull` is what stops a caller dereferencing either.
    let addr = sys::sys_result(unsafe {
        sys::syscall6(
            sys::SYS_MMAP,
            addr,
            length,
            prot as u64,
            flags as u64,
            r8,
            r9,
        )
    })?;
    NonNull::new(addr as *mut u8).ok_or(Errno::ENOMEM)
}

/// Unmap memory from the address space.
pub fn munmap(addr: *mut u8, length: u64) -> Result<(), Errno> {
    sys::sys_ok(unsafe { sys::syscall2(sys::SYS_MUNMAP, addr as u64, length) })
}

/// Change the protection of pages already mapped.
///
/// `addr` must be page-aligned and the whole range must be mapped; a hole
/// anywhere in it leaves every page as it was and reports `ENOMEM`.
pub fn mprotect(addr: *mut u8, length: u64, prot: u32) -> Result<(), Errno> {
    sys::sys_ok(unsafe { sys::syscall3(sys::SYS_MPROTECT, addr as u64, length, prot as u64) })
}

/// Flush or invalidate a range of memory-mapped file pages.
///
/// `flags` should be one of `MS_ASYNC`, `MS_SYNC`, or `MS_INVALIDATE`.
///
/// # Safety
/// `addr` must point to a page-aligned address within a file-backed mapping of
/// at least `length` bytes.
pub unsafe fn msync(addr: *mut u8, length: u64, flags: u32) -> Result<(), Errno> {
    sys::sys_ok(unsafe { sys::syscall3(sys::SYS_MSYNC, addr as u64, length, flags as u64) })
}
