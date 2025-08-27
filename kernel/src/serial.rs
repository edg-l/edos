use core::fmt;

use spin::{Once, mutex::Mutex};
use uart_16550::SerialPort;

static SERIAL_DBG: Once<Mutex<SerialPort>> = Once::new();

pub fn init() {
    SERIAL_DBG.call_once(|| {
        let mut port = unsafe { uart_16550::SerialPort::new(0x3F8) };
        port.init();
        Mutex::new(port)
    });
}

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => ($crate::serial::_serial_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! serial_println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}

#[doc(hidden)]
pub fn _serial_print(args: fmt::Arguments) {
    use core::fmt::Write;
    //let uptime_us = time::uptime_us();
    //let secs = uptime_us / 1_000_000;
    //let us = uptime_us % 1_000_000;
    //serial()
    //    .write_fmt(format_args!("[{secs}.{us:06}] "))
    //    .unwrap();
    unsafe {
        SERIAL_DBG
            .get()
            .unwrap_unchecked()
            .lock()
            .write_fmt(args)
            .unwrap();
    }
}
