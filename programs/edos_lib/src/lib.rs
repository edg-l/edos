pub mod args;
pub mod clipboard;
pub mod config;
pub mod io;
pub mod keymap;
pub mod mem;
pub mod mounts;
pub mod net;
pub mod process;
pub mod procinfo;
pub mod profile;
pub mod shm;
pub mod sync;
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

/// Fill `buf` with random bytes from the kernel.
///
/// The error carries nothing: `getrandom` here fails only when the kernel has
/// no entropy source at all, which is not a condition a caller can act on
/// differently from any other.
#[expect(
    clippy::result_unit_err,
    reason = "the failure carries nothing a caller could act on: no entropy source at all"
)]
pub fn try_getrandom(buf: &mut [u8]) -> Result<(), ()> {
    edos_rt::io::getrandom(buf).map(|_| ()).map_err(|_| ())
}
