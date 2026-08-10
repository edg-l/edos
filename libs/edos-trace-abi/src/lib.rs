//! The ABI between the kernel's syscall tracer and userspace `strace`.
//!
//! Both sides link this crate, so the record layout and the argument encoding
//! `/proc/syscalls` publishes cannot drift apart: a change here fails to
//! compile on whichever side was not updated.

#![no_std]

/// Bytes of inline string storage a record carries, shared by both captured
/// arguments.
pub const TRACE_STR_CAP: usize = 160;

/// The kernel could not read a string argument out of user memory. Stored in
/// `TraceRecord::str_lens` in place of a length, so a fault is distinguishable
/// from an empty string.
pub const TRACE_STR_FAULT: u16 = u16::MAX;

/// The argument was readable but an earlier one had already filled the record.
/// Distinct from [`TRACE_STR_FAULT`], which means the address was bad.
pub const TRACE_STR_TRUNCATED: u16 = u16::MAX - 1;

/// Entry into a syscall: `args` and the captured strings are valid.
pub const TRACE_ENTER: u32 = 0;
/// Return from a syscall: `ret` and `errno` are valid.
pub const TRACE_EXIT: u32 = 1;
/// The thread ended. `ret` carries the exit code; no syscall is in flight.
pub const TRACE_DIED: u32 = 2;

/// One traced event.
///
/// Fixed size and `Copy` so the kernel ring is a plain array and draining it
/// is one `try_copy_to_user` of `n * size_of::<TraceRecord>()` bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TraceRecord {
    pub tid: u64,
    /// Nanoseconds since boot.
    pub time_ns: u64,
    /// The six syscall argument registers, whatever the call's arity.
    pub args: [u64; 6],
    pub ret: u64,
    pub nr: u32,
    /// One of `TRACE_ENTER`, `TRACE_EXIT`, `TRACE_DIED`.
    pub kind: u32,
    /// `Errno` as the kernel reports it, valid on `TRACE_EXIT`.
    pub errno: u32,
    /// Lengths of the captured strings within `strs`, or [`TRACE_STR_FAULT`].
    /// They are packed back to back, so the second starts at `str_lens[0]`.
    pub str_lens: [u16; 2],
    pub strs: [u8; TRACE_STR_CAP],
}

impl TraceRecord {
    pub const fn zeroed() -> Self {
        Self {
            tid: 0,
            time_ns: 0,
            args: [0; 6],
            ret: 0,
            nr: 0,
            kind: TRACE_ENTER,
            errno: 0,
            str_lens: [0; 2],
            strs: [0; TRACE_STR_CAP],
        }
    }

    /// The `slot`-th captured string, or `None` if that slot holds no string
    /// or the kernel faulted reading it.
    pub fn captured_str(&self, slot: usize) -> Option<&[u8]> {
        let len = *self.str_lens.get(slot)?;
        if !Self::is_length(len) {
            return None;
        }
        let start = if slot == 0 {
            0
        } else {
            let prev = self.str_lens[slot - 1];
            if Self::is_length(prev) { prev as usize } else { 0 }
        };
        self.strs.get(start..start + len as usize)
    }

    /// Whether a `str_lens` entry is a real length rather than a sentinel.
    const fn is_length(len: u16) -> bool {
        len != TRACE_STR_FAULT && len != TRACE_STR_TRUNCATED
    }
}

/// What one syscall argument is, as `/proc/syscalls` reports it.
///
/// The kernel uses this to decide which arguments to copy out of user memory;
/// `strace` uses it to decide how to print them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgKind {
    /// Signed decimal.
    Int,
    /// Hexadecimal: flags, modes, requests.
    Hex,
    /// File descriptor. Decimal, with `AT_FDCWD` named.
    Fd,
    /// Address the kernel does not read through.
    Ptr,
    /// NUL-terminated string in user memory. Captured.
    Str,
    /// String or data buffer in user memory whose length is the *following*
    /// argument. Captured on entry.
    StrLen,
    /// Buffer the syscall fills, whose filled length is its return value.
    /// Captured on return, into that record's first string slot.
    Out,
    /// Buffer whose contents are not captured.
    Buf,
    /// Unsigned count or size.
    Len,
}

impl ArgKind {
    pub const fn as_char(self) -> char {
        match self {
            ArgKind::Int => 'd',
            ArgKind::Hex => 'x',
            ArgKind::Fd => 'f',
            ArgKind::Ptr => 'p',
            ArgKind::Str => 's',
            ArgKind::StrLen => 'S',
            ArgKind::Out => 'o',
            ArgKind::Buf => 'b',
            ArgKind::Len => 'n',
        }
    }

    pub const fn from_char(c: char) -> Option<Self> {
        match c {
            'd' => Some(ArgKind::Int),
            'x' => Some(ArgKind::Hex),
            'f' => Some(ArgKind::Fd),
            'p' => Some(ArgKind::Ptr),
            's' => Some(ArgKind::Str),
            'S' => Some(ArgKind::StrLen),
            'o' => Some(ArgKind::Out),
            'b' => Some(ArgKind::Buf),
            'n' => Some(ArgKind::Len),
            _ => None,
        }
    }

    /// Whether the kernel copies this argument's target out of user memory on
    /// the way in.
    pub const fn is_captured(self) -> bool {
        matches!(self, ArgKind::Str | ArgKind::StrLen)
    }

    /// Whether the kernel copies this argument's target on the way out, sized
    /// by the syscall's return value.
    pub const fn is_out(self) -> bool {
        matches!(self, ArgKind::Out)
    }
}

/// `trace_ctl` operations.
pub mod ctl {
    /// Become the tracer. Allocates the ring and invalidates every mark a
    /// previous tracer left behind. Fails if another live thread holds it.
    pub const CLAIM: u64 = 0;
    /// Stop tracing everything and free the ring. Tracer only.
    pub const RELEASE: u64 = 1;
    /// Mark a thread traced; `arg` is a tid, or 0 for the caller.
    pub const MARK: u64 = 2;
    /// Stop tracing one thread; `arg` is a tid, or 0 for the caller.
    pub const UNMARK: u64 = 3;
    /// Return how many records have been dropped because the ring was full.
    pub const DROPPED: u64 = 4;
}
