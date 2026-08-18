//! Progress on the kernel log rather than on stdout.
//!
//! `println!` reaches the GUI terminal the installer runs in and nothing
//! else: the serial log is where an unattended run is judged from, and it
//! never sees a word of it. Every phase, and every path the copy is about to
//! write, goes here as well, so an install that stops halfway names the file
//! it stopped on instead of leaving a cursor blinking under
//! "Copying the system...".

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Opened once: the copy traces one line per file, and reopening `/dev/klog`
/// for each of them would time the install's own tracing rather than the
/// install.
fn sink() -> Option<&'static Mutex<std::fs::File>> {
    static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    SINK.get_or_init(|| {
        OpenOptions::new()
            .write(true)
            .open("/dev/klog")
            .ok()
            .map(Mutex::new)
    })
    .as_ref()
}

/// Write one line to `/dev/klog`. Silently does nothing when the device is
/// missing: tracing must never be the reason an install fails.
pub fn trace(msg: &str) {
    if let Some(sink) = sink()
        && let Ok(mut file) = sink.lock()
    {
        let _ = writeln!(file, "edos-install: {msg}");
    }
}
