use core::fmt::{self, Write};

use spin::{Once, mutex::Mutex};
use uart_16550::SerialPort;

use crate::{acpi::current_cpu_index, timer::uptime_us};

static SERIAL_DBG: Once<Mutex<SerialPort>> = Once::new();

pub fn init() {
    SERIAL_DBG.call_once(|| {
        let mut port = unsafe { uart_16550::SerialPort::new(0x3F8) };
        port.init();
        Mutex::new(port)
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

    unsafe {
        SERIAL_DBG
            .get()
            .unwrap_unchecked()
            .lock()
            .write_fmt(format_args!(
                "[{secs}.{us:06}] <cpu-{}:kernel> {args}",
                current_cpu_index(),
            ))
            .unwrap();
    }
}

pub fn add_serial_log(text: &str) {
    SERIAL_DBG.get().unwrap().lock().write_str(text).unwrap();
}
