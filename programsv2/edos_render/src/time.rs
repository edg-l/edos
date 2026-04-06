//! Wall-clock time via the RTC syscall.

use std::arch::asm;

const SYS_CLOCK_GETTIME: u64 = 226;

/// Wall-clock time read from the hardware RTC.
#[derive(Debug, Clone, Copy)]
pub struct ClockTime {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub day: u8,
    pub month: u8,
    pub year: u16,
}

/// Read the current wall-clock time from the RTC via syscall 226.
///
/// Returns `None` if the syscall fails.
pub fn clock_gettime() -> Option<ClockTime> {
    let mut buf = [0u8; 8];
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") SYS_CLOCK_GETTIME,
            in("rdi") buf.as_mut_ptr() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if ret == !0u64 {
        return None;
    }
    // buf layout: [hour, minute, second, 0, day, month, year_lo, year_hi]
    let year = u16::from_le_bytes([buf[6], buf[7]]);
    Some(ClockTime {
        hour: buf[0],
        minute: buf[1],
        second: buf[2],
        day: buf[4],
        month: buf[5],
        year,
    })
}
