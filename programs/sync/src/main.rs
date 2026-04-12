use std::arch::asm;

const SYS_SYNC: u64 = 162;

fn main() {
    unsafe {
        asm!(
            "syscall",
            in("rax") SYS_SYNC,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
}
