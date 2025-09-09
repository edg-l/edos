// Syscall numbers
pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_CLOSE: u64 = 3;
pub const SYS_MMAP: u64 = 9;
pub const SYS_MUNMAP: u64 = 11;
pub const SYS_GETPID: u64 = 39;
pub const SYS_EXIT: u64 = 60;
pub const SYS_ERRNO: u64 = 0x400;
pub const SYS_DRAW_RECT: u64 = 100;
pub const SYS_RENDER: u64 = 101;
pub const SYS_SCREEN_INFO: u64 = 102;
pub const SYS_DRAW: u64 = 103;
pub const SYS_RAW_INPUT: u64 = 200;
pub const SYS_KERNEL_LOGS: u64 = 201;

// Memory protection flags
pub const PROT_READ: u32 = 0x1;
pub const PROT_WRITE: u32 = 0x2;
pub const PROT_EXEC: u32 = 0x4;

// Memory mapping flags
pub const MAP_PRIVATE: u32 = 0x02;
pub const MAP_ANONYMOUS: u32 = 0x20;
