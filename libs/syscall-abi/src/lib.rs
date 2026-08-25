//! The syscall boundary's `#[repr(C)]` types: the structs the kernel writes
//! into a user buffer, or reads out of one.
//!
//! None of these can be checked at compile time from one side alone. A field
//! added on one side and not the other is read at the wrong offsets, and an
//! enumerated field whose meaning is documented twice can be documented
//! wrongly on the side that defines it — `DirEntry::file_type` was, for a
//! while, `4=device` in the kernel and `4=fifo` in userspace, and only the
//! second was true. Both halves live here now, so there is one layout and one
//! statement of what each field means.
//!
//! Nothing that issues a syscall belongs here. `edos_lib` wraps the calls and
//! the kernel implements them; this is only the shapes they exchange.

#![no_std]

/// What a descriptor is ready for, both as a poll interest and as a result.
///
/// Error, hang-up and invalid are output-only: [`matches`](Self::matches)
/// reports them whether or not the caller asked for them, per POSIX.1-2024
/// `poll` (POLLERR, POLLHUP, POLLNVAL).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PollState {
    pub readable: bool,
    pub writable: bool,
    pub error: bool,
    pub hangup: bool,
    pub invalid: bool,
}

impl PollState {
    pub const fn none() -> Self {
        Self {
            readable: false,
            writable: false,
            error: false,
            hangup: false,
            invalid: false,
        }
    }

    /// Whether this state makes a poll on `interests` ready.
    ///
    /// A caller waiting for data on a descriptor whose peer has gone away
    /// would otherwise wait forever for a read that would return end of file
    /// immediately, so the output-only conditions always match.
    pub fn matches(&self, interests: Self) -> bool {
        if self.error || self.hangup || self.invalid {
            return true;
        }

        let mut matched = false;

        if interests.readable && self.readable {
            matched = true;
        }
        if interests.writable && self.writable {
            matched = true;
        }

        if !interests.readable && !interests.writable {
            matched = self.readable || self.writable;
        }

        matched
    }

    pub const fn to_bits(self) -> u8 {
        (self.readable as u8)
            | ((self.writable as u8) << 1)
            | ((self.error as u8) << 2)
            | ((self.hangup as u8) << 3)
            | ((self.invalid as u8) << 4)
    }

    pub const fn from_bits(bits: u8) -> Self {
        Self {
            readable: (bits & 0x01) != 0,
            writable: (bits & 0x02) != 0,
            error: (bits & 0x04) != 0,
            hangup: (bits & 0x08) != 0,
            invalid: (bits & 0x10) != 0,
        }
    }
}

/// One descriptor in a `poll` array: the caller fills `interests`, the kernel
/// fills `result`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SelectFd {
    pub fd: u64,
    pub interests: PollState,
    pub result: PollState,
}

impl SelectFd {
    pub const EMPTY: Self = Self {
        fd: 0,
        interests: PollState::none(),
        result: PollState::none(),
    };
}

/// One directory entry as `getdents` writes it, immediately followed in the
/// buffer by `name_len` bytes of name.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DirEntry {
    pub name_len: u32,
    /// 0=file, 1=directory, 2=symlink, 3=special, 4=fifo.
    pub file_type: u8,
    pub size: u64,
    /// readonly=1, hidden=2, system=4, archive=8.
    pub attrs: u8,
    pub reserved: [u8; 2],
}

/// What `stat`, `fstat` and `fstatat` report about a file.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Stat {
    pub size: u64,
    /// Creation, access and modification times, in whole Unix seconds.
    pub created: u64,
    pub accessed: u64,
    pub modified: u64,
    /// readonly=1, hidden=2, system=4, archive=8.
    pub attrs: u16,
    /// 0=file, 1=directory, 2=symlink, 3=special, 4=fifo.
    pub kind: u8,
}

/// What `statfs` writes: how much room a filesystem has and what it calls
/// itself, with both names as NUL-padded fixed-width bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RawStatFs {
    pub fs_type: [u8; 16],
    pub block_size: u64,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
    pub volume_name: [u8; 64],
    pub version: u32,
    pub block_groups: u16,
    pub _pad: [u8; 2],
}

/// The address family `SockAddrIn` carries. IPv4 is the only one the stack
/// implements, and it takes the BSD number.
pub const AF_INET: u16 = 2;

/// `sockaddr_in`: the address `bind`, `connect`, `sendto` and `recvfrom`
/// exchange.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrIn {
    pub family: u16,
    /// Network byte order.
    pub port: u16,
    pub addr: [u8; 4],
    pub zero: [u8; 8],
}

impl SockAddrIn {
    /// `port` is given in host byte order and stored big-endian.
    pub const fn new(addr: [u8; 4], port: u16) -> Self {
        Self {
            family: AF_INET,
            port: port.to_be(),
            addr,
            zero: [0; 8],
        }
    }
}
