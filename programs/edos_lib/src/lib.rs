pub mod clipboard;
pub mod config;
pub mod io;
pub mod keymap;
pub mod mem;
pub mod net;
pub mod process;
pub mod procinfo;
pub mod shm;
pub mod sys;
pub mod term;
pub mod time;
pub mod trace;

/// Fill `buf` with random bytes from the kernel.
pub fn getrandom(buf: &mut [u8]) {
    let _ = edos_rt::io::getrandom(buf);
}
