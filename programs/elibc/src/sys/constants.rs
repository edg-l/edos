// Syscall numbers
pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_OPEN: u64 = 2;
pub const SYS_CLOSE: u64 = 3;
pub const SYS_LIST_DIR: u64 = 4;
pub const SYS_GETCWD: u64 = 5;
pub const SYS_CHDIR: u64 = 6;
pub const SYS_POLL: u64 = 7;
pub const SYS_FSTAT: u64 = 8;
pub const SYS_MMAP: u64 = 9;
pub const SYS_STAT: u64 = 10;
pub const SYS_MUNMAP: u64 = 11;
pub const SYS_SELECT: u64 = 23;
pub const SYS_IOCTL: u64 = 16;
pub const SYS_PIPE: u64 = 22;
pub const SYS_DUP2: u64 = 33;
pub const SYS_GETPID: u64 = 39;
pub const SYS_WAIT_PID: u64 = 40;
pub const SYS_SPAWN: u64 = 57;
pub const SYS_EXIT: u64 = 60;
pub const SYS_ERRNO: u64 = 0x400;
pub const SYS_MOUNT: u64 = 202;
pub const SYS_LIST_PARTITIONS: u64 = 203;
pub const SYS_MKDIR: u64 = 204;
pub const SYS_RMDIR: u64 = 205;
pub const SYS_RMDIR_ALL: u64 = 206;
pub const SYS_UNLINK: u64 = 207;
pub const SYS_LIST_MOUNTS: u64 = 208;
pub const SYS_SLEEP_MS: u64 = 209;
pub const SYS_MONOTONIC_TIME: u64 = 210;
pub const SYS_CLONE: u64 = 211;
pub const SYS_FUTEX_WAIT: u64 = 212;
pub const SYS_FUTEX_WAKE: u64 = 213;

// Ioctl buffer flags
pub const IOCTL_ARG_IN: u64 = 1;
pub const IOCTL_ARG_OUT: u64 = 1 << 1;

// Memory protection flags
pub const PROT_READ: u32 = 0x1;
pub const PROT_WRITE: u32 = 0x2;
pub const PROT_EXEC: u32 = 0x4;

// Memory mapping flags
pub const MAP_PRIVATE: u32 = 0x02;
pub const MAP_ANONYMOUS: u32 = 0x20;
