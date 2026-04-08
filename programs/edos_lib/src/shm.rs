//! Shared memory syscall wrappers.

use crate::sys::{self, SYS_SHM_CREATE, SYS_SHM_DESTROY, SYS_SHM_MAP, SYS_SHM_SIZE, SYS_SHM_UNMAP};

// Protection flags for shm_map
pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

/// Create a new shared memory region.
///
/// Returns the shared memory ID on success.
pub fn shm_create(size: usize) -> Result<u64, i64> {
    let result = unsafe { sys::syscall1(SYS_SHM_CREATE, size as u64) };
    if result as i64 == -1 {
        Err(-1)
    } else {
        Ok(result)
    }
}

/// Map a shared memory region into the calling process's address space.
///
/// Returns a pointer to the mapped memory on success.
pub fn shm_map(shm_id: u64, prot: u64) -> Result<*mut u8, i64> {
    let result = unsafe { sys::syscall3(SYS_SHM_MAP, shm_id, 0, prot) };
    if result as i64 == -1 {
        Err(-1)
    } else {
        Ok(result as *mut u8)
    }
}

/// Unmap a shared memory region from the calling process's address space.
pub fn shm_unmap(addr: *mut u8) -> Result<(), i64> {
    let result = unsafe { sys::syscall1(SYS_SHM_UNMAP, addr as u64) };
    if result as i64 == -1 { Err(-1) } else { Ok(()) }
}

/// Destroy a shared memory region.
pub fn shm_destroy(shm_id: u64) -> Result<(), i64> {
    let result = unsafe { sys::syscall1(SYS_SHM_DESTROY, shm_id) };
    if result as i64 == -1 { Err(-1) } else { Ok(()) }
}

/// Get the allocated size of a shared memory region in bytes.
pub fn shm_size(shm_id: u64) -> Result<usize, i64> {
    let result = unsafe { sys::syscall1(SYS_SHM_SIZE, shm_id) };
    if result as i64 == -1 {
        Err(-1)
    } else {
        Ok(result as usize)
    }
}
