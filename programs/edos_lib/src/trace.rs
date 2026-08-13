//! Syscall tracing: claim the kernel tracer, mark threads, drain records.
//!
//! The record layout and the argument encoding come from `edos_trace_abi`,
//! which the kernel links too.

use crate::sys;

pub use edos_trace_abi::{
    ArgKind, TRACE_DIED, TRACE_ENTER, TRACE_EXIT, TRACE_STR_FAULT, TRACE_STR_TRUNCATED,
    TraceRecord, ctl,
};

/// Become the tracer, allocating the kernel's record ring.
///
/// Fails while another live process holds the session. The session ends when
/// [`release`] is called or the caller exits, whichever comes first.
pub fn claim() -> bool {
    unsafe { sys::syscall2(sys::SYS_TRACE_CTL, ctl::CLAIM, 0) == 0 }
}

/// End the session and free the ring. Tracer only.
pub fn release() -> bool {
    unsafe { sys::syscall2(sys::SYS_TRACE_CTL, ctl::RELEASE, 0) == 0 }
}

/// Start recording `tid`'s syscalls; 0 means the calling thread.
///
/// Children a marked thread creates are marked too, so a shell script under
/// trace shows the programs it runs.
pub fn mark(tid: u64) -> bool {
    unsafe { sys::syscall2(sys::SYS_TRACE_CTL, ctl::MARK, tid) == 0 }
}

/// Stop recording `tid`'s syscalls; 0 means the calling thread.
pub fn unmark(tid: u64) -> bool {
    unsafe { sys::syscall2(sys::SYS_TRACE_CTL, ctl::UNMARK, tid) == 0 }
}

/// Records the kernel discarded because the ring was full and this tracer was
/// not draining it fast enough.
pub fn dropped() -> u64 {
    unsafe { sys::syscall2(sys::SYS_TRACE_CTL, ctl::DROPPED, 0) }
}

/// Drain up to `buf.len()` records, parking for at most `timeout_ms` if none
/// are waiting.
///
/// Returns the number of records written, which is 0 on timeout.
pub fn read(buf: &mut [TraceRecord], timeout_ms: u64) -> usize {
    let n = unsafe {
        sys::syscall3(
            sys::SYS_TRACE_READ,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            timeout_ms,
        )
    };
    if n == u64::MAX { 0 } else { n as usize }
}

/// One row of `/proc/syscalls`: what the kernel calls a number, and what its
/// arguments are.
pub struct SyscallInfo {
    pub name: String,
    pub args: Vec<ArgKind>,
}

/// The syscall surface as the kernel describes it.
#[derive(Default)]
pub struct SyscallTable {
    pub calls: std::collections::BTreeMap<u32, SyscallInfo>,
    pub errnos: std::collections::BTreeMap<u32, String>,
}

impl SyscallTable {
    pub fn name_of(&self, nr: u32) -> String {
        self.calls
            .get(&nr)
            .map(|info| info.name.clone())
            .unwrap_or_else(|| format!("syscall_{nr}"))
    }

    pub fn errno_name(&self, value: u32) -> Option<&str> {
        self.errnos.get(&value).map(|s| s.as_str())
    }

    /// The number the kernel gives an error code by name, which is what a
    /// program needs to recognise a code its runtime's `Errno` list predates.
    pub fn errno_value(&self, name: &str) -> Option<u32> {
        self.errnos
            .iter()
            .find(|(_, known)| known.as_str() == name)
            .map(|(value, _)| *value)
    }
}

/// Parse `/proc/syscalls`.
///
/// The kernel renders it from the same table its tracer captures arguments
/// with, so a syscall added there needs no change here.
pub fn read_syscall_table() -> SyscallTable {
    let mut table = SyscallTable::default();
    let Ok(text) = std::fs::read_to_string("/proc/syscalls") else {
        return table;
    };

    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(tag) = fields.next() else { continue };
        match tag {
            "call" => {
                let (Some(nr), Some(name), Some(kinds)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                let Ok(nr) = nr.parse::<u32>() else { continue };
                table.calls.insert(
                    nr,
                    SyscallInfo {
                        name: name.to_string(),
                        args: kinds.chars().filter_map(ArgKind::from_char).collect(),
                    },
                );
            }
            "errno" => {
                let (Some(value), Some(name)) = (fields.next(), fields.next()) else {
                    continue;
                };
                let Ok(value) = value.parse::<u32>() else {
                    continue;
                };
                table.errnos.insert(value, name.to_string());
            }
            _ => {}
        }
    }
    table
}
