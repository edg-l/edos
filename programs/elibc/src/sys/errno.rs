use crate::sys::{calls::syscall0, constants::SYS_ERRNO};

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::upper_case_acronyms)]
#[repr(u64)]
pub enum Errno {
    Clear,
    EINVAL,
    ENOMEM,
    EFAULT,
    UNKNOWN,
}

pub fn errno() -> Errno {
    unsafe { core::mem::transmute(syscall0(SYS_ERRNO)) }
}
