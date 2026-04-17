use core::fmt::{self, Write};

use spin::Once;
use uart_16550::SerialPort;

use crate::{thread::irqlock::IrqSpinlock, timer::uptime_us, util::per_cpu::get_percpu_data};

static SERIAL_DBG: Once<IrqSpinlock<SerialPort>> = Once::new();

pub fn init() {
    SERIAL_DBG.call_once(|| {
        let mut port = unsafe { uart_16550::SerialPort::new(0x3F8) };
        port.init();
        IrqSpinlock::new(port)
    });
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::serial::_serial_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

#[doc(hidden)]
pub fn _serial_print(args: fmt::Arguments) {
    use core::fmt::Write;
    let uptime_us = uptime_us();
    let secs = uptime_us / 1_000_000;
    let us = uptime_us % 1_000_000;

    let lapic_id = get_percpu_data().lapic_id.get();

    SERIAL_DBG
        .get()
        .expect("failed to get serial dbg in print")
        .lock()
        .write_fmt(format_args!(
            "[{secs}.{us:06}] <cpu-{}:kernel> {args}",
            lapic_id,
        ))
        .expect("write fmt failed in serial");
}

pub fn add_serial_log(text: &str) {
    SERIAL_DBG
        .get()
        .expect("failed to get serial dbg")
        .lock()
        .write_str(text)
        .expect("write_str failed in serial");
}

/// Emergency serial output that bypasses all locks. Use only in double fault
/// or other unrecoverable handlers where the serial lock may already be held.
/// Writes directly to the 0x3F8 UART data port.
pub fn emergency_write(msg: &[u8]) {
    for &byte in msg {
        unsafe {
            // Spin-wait for transmit buffer empty (LSR bit 5)
            while x86_64::instructions::port::Port::<u8>::new(0x3FD).read() & 0x20 == 0 {
                core::hint::spin_loop();
            }
            x86_64::instructions::port::Port::<u8>::new(0x3F8).write(byte);
        }
    }
}

/// Lock-bypassing formatted print for crash paths. Formats into a 512-byte
/// stack buffer (truncates silently on overflow) then writes directly to the
/// UART. Use from page-fault/double-fault handlers that might race with the
/// regular serial lock.
#[doc(hidden)]
pub fn _emergency_print(args: fmt::Arguments) {
    let mut buf: heapless::String<512> = heapless::String::new();
    let _ = buf.write_fmt(args);
    emergency_write(buf.as_bytes());
}

#[macro_export]
macro_rules! emergency_println {
    ($($arg:tt)*) => {
        $crate::serial::_emergency_print(core::format_args!("{}\n", core::format_args!($($arg)*)))
    };
}
