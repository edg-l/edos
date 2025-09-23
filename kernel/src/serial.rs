use core::fmt::{self, Write};

use spin::{Once, mutex::Mutex};
use uart_16550::SerialPort;
use x86_64::instructions::interrupts::without_interrupts;

use crate::{timer::uptime_us, util::per_cpu::get_percpu_data};

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

    let lapic_id = get_percpu_data().lapic_id;

    without_interrupts(|| {
        SERIAL_DBG
            .get()
            .expect("failed to get serial dbg in print")
            .lock()
            .write_fmt(format_args!(
                "[{secs}.{us:06}] <cpu-{}:kernel> {args}",
                lapic_id,
            ))
            .expect("write fmt failed in serial");
    })
}

pub fn add_serial_log(text: &str) {
    without_interrupts(|| {
        SERIAL_DBG
            .get()
            .expect("failed to get serial dbg")
            .lock()
            .write_str(text)
            .expect("write_str failed in serial");
    })
}
