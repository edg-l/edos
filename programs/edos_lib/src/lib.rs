pub mod clipboard;
pub mod config;
pub mod io;
pub mod keymap;
pub mod mem;
pub mod mounts;
pub mod net;
pub mod process;
pub mod procinfo;
pub mod shm;
pub mod sys;
pub mod term;
pub mod time;
pub mod trace;

/// Fill `buf` with random bytes from the kernel.
///
/// A failed syscall leaves `buf` as the caller passed it. Anything deriving a
/// key, a nonce or a challenge from these bytes must use [`try_getrandom`]
/// instead, since silently keeping a zeroed buffer is indistinguishable from
/// success and produces a predictable secret.
pub fn getrandom(buf: &mut [u8]) {
    let _ = edos_rt::io::getrandom(buf);
}

/// Fill `buf` with random bytes from the kernel, reporting failure.
pub fn try_getrandom(buf: &mut [u8]) -> Result<(), ()> {
    edos_rt::io::getrandom(buf).map(|_| ()).map_err(|_| ())
}
