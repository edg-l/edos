pub mod io;
pub mod keymap;
pub mod net;
pub mod process;
pub mod shm;
pub mod sys;
pub mod time;

/// Fill `buf` with random bytes from the kernel.
pub fn getrandom(buf: &mut [u8]) {
    unsafe {
        sys::syscall3(
            214, // SYS_GETRANDOM
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            0,
        );
    }
}
