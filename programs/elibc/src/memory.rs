use crate::sys::{
    calls::{syscall2, syscall4},
    constants::*,
};

/// Map memory into the address space
/// # Safety
/// Caller must ensure parameters are valid for mmap semantics
pub fn mmap(addr: *mut u8, length: u64, prot: u32, flags: u32) -> *mut u8 {
    unsafe { syscall4(SYS_MMAP, addr as u64, length, prot as u64, flags as u64) as *mut u8 }
}

/// Unmap memory from the address space
pub fn munmap(addr: *mut u8, length: u64) -> i32 {
    unsafe { syscall2(SYS_MUNMAP, addr as u64, length) as i32 }
}
