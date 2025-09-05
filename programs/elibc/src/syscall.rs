#![expect(clippy::missing_safety_doc)]

use core::arch::asm;

/// Raw syscall with 3 arguments
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall3(num: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            lateout("rax") result, // rcx is clobbered by syscall (contains user RIP)
            lateout("rcx") _, // rcx is clobbered by syscall (contains user RIP)
            lateout("r11") _, // r11 is clobbered by syscall (contains user RFLAGS)
            options(nostack, preserves_flags)
        );
    }
    result
}

/// Raw syscall with 1 argument
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall1(num: u64, arg1: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags)
        );
    }
    result
}

/// Raw syscall with no argument
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall0(num: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags)
        );
    }
    result
}

/// Raw syscall with 6 arguments
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall6(
    num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
            in("r9") arg6,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags)
        );
    }
    result
}

/// Raw syscall with 2 arguments
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall2(num: u64, arg1: u64, arg2: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags)
        );
    }
    result
}

/// Raw syscall with 6 arguments
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall4(num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            lateout("rax") result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags)
        );
    }
    result
}
