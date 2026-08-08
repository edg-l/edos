use edos_lib::sys::{SYS_SYNC, syscall0};

fn main() {
    unsafe { syscall0(SYS_SYNC) };
}
