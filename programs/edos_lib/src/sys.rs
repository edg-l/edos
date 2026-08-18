//! Raw syscall helpers and syscall number constants.
//!
//! Numbers and raw syscall wrappers common to every edos userspace program
//! live in `edos_rt`; re-exported here rather than redefined, so there is one
//! place that can get a syscall number wrong instead of two. Only the
//! constants and helpers with no `edos_rt` equivalent are defined below.

pub use edos_rt::sys::{
    Errno, MAX_ERRNO, SYS_ACCEPT, SYS_ACCESS, SYS_BIND, SYS_CLOCK_GETTIME, SYS_CLOSE, SYS_CONNECT,
    SYS_DUP, SYS_DUP2, SYS_ERRNO, SYS_FCNTL, SYS_FORK, SYS_FSTATAT, SYS_FTRUNCATE, SYS_GETDENTS,
    SYS_GETPID, SYS_GETSOCKOPT, SYS_IOCTL, SYS_ISATTY, SYS_KILL, SYS_LIST_MOUNTS,
    SYS_LIST_PARTITIONS, SYS_LISTEN, SYS_LSEEK, SYS_MMAP, SYS_MOUNT, SYS_MSYNC, SYS_MUNMAP,
    SYS_NANOSLEEP, SYS_OPEN, SYS_OPENAT, SYS_PING, SYS_PIPE, SYS_POLL, SYS_READ, SYS_READLINK,
    SYS_READV, SYS_RECVFROM, SYS_RENAME, SYS_SCHED_YIELD, SYS_SENDTO, SYS_SETSOCKOPT, SYS_SHUTDOWN,
    SYS_SIGACTION, SYS_SOCKET, SYS_SPAWN, SYS_SPAWN2, SYS_STAT, SYS_STATFS, SYS_SYMLINK, SYS_SYNC,
    SYS_UTIMENSAT, SYS_WAIT_PID, SYS_WRITE, SYS_WRITEV, errno as raw_errno, is_err, sys_result,
    syscall0, syscall1, syscall2, syscall3, syscall4, syscall5, syscall6,
};

pub const SYS_MPROTECT: u64 = 289;
pub const SYS_TRUNCATE: u64 = 76;
pub const SYS_MKDIRAT: u64 = 258;
pub const SYS_MKFIFOAT: u64 = 283;
pub const SYS_UNLINKAT: u64 = 263;
pub const SYS_RENAMEAT: u64 = 264;
pub const SYS_SYMLINKAT: u64 = 266;
pub const SYS_READLINKAT: u64 = 267;
pub const SYS_FACCESSAT: u64 = 269;
pub const SYS_PREAD: u64 = 17;
pub const SYS_PWRITE: u64 = 18;
pub const SYS_EXECVE: u64 = 59;
pub const SYS_GETUID: u64 = 102;
pub const SYS_GETGID: u64 = 104;
pub const SYS_CLOCK_SETTIME: u64 = 281;
pub const SYS_SCHED_SETATTR: u64 = 314;
pub const SYS_SCHED_GETATTR: u64 = 315;
pub const SYS_CLIPBOARD_GET: u64 = 284;
pub const SYS_CLIPBOARD_SET: u64 = 285;
pub const SYS_OPENPTY: u64 = 227;
pub const SYS_SHM_CREATE: u64 = 215;
pub const SYS_SHM_MAP: u64 = 216;
pub const SYS_SHM_UNMAP: u64 = 217;
pub const SYS_SHM_DESTROY: u64 = 218;
pub const SYS_SHM_SIZE: u64 = 231;
pub const SYS_SIGPROCMASK: u64 = 233;
pub const SYS_SETPGID: u64 = 109;
pub const SYS_GETPGID: u64 = 121;
pub const SYS_TCSETPGRP: u64 = 237;
pub const SYS_TCGETPGRP: u64 = 238;
pub const SYS_SIGRETURN: u64 = 239;
pub const SYS_TRACE_CTL: u64 = 235;
pub const SYS_TRACE_READ: u64 = 236;
pub const SYS_NETINFO: u64 = 250;
pub const SYS_GETDNS: u64 = 256;
pub const SYS_SETDNS: u64 = 316;
pub const SYS_REBOOT: u64 = 169;
pub const SYS_GETTID: u64 = 186;
pub const SYS_FUTEX_WAKE: u64 = 213;
pub const SYS_FUTEX_WAIT_PI: u64 = 317;

pub const AF_INET: u32 = 2;
pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;

pub const SOL_SOCKET: u32 = 1;
pub const SO_ERROR: u32 = 4;
pub const SO_RCVTIMEO: u32 = 20;
