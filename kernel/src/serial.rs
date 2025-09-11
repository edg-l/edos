use core::fmt;

use alloc::{format, string::String, sync::Arc};
use spin::{Once, mutex::Mutex};
use uart_16550::SerialPort;
use x86_64::instructions::interrupts::without_interrupts;

use crate::{
    acpi::current_cpu_index,
    thread::broadcast::{LockedBroadcast, new_broadcast},
    timer::uptime_us,
    util::per_cpu::get_percpu_data,
};

static SERIAL_DBG: Once<Mutex<SerialPort>> = Once::new();

pub static SERIAL_SUBSCRIBER: LockedBroadcast<Arc<String>> = new_broadcast(256, true);

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
    without_interrupts(|| {
        use core::fmt::Write;
        let uptime_us = uptime_us();
        let secs = uptime_us / 1_000_000;
        let us = uptime_us % 1_000_000;

        if let Some(sched) = unsafe { get_percpu_data().scheduler.as_mut() }
            && let Some(tid) = sched.current_id_opt()
        {
            let text = format!(
                "[{secs}.{us:06}] <cpu-{}:{}:{}:{}> {args}",
                current_cpu_index(),
                tid.name
                    .as_ref()
                    .map(|x| x.as_str())
                    .unwrap_or_else(|| if tid.kernel { "unk0" } else { "unk3" }),
                if tid.kernel { "k" } else { "u" },
                tid.id
            );
            unsafe {
                SERIAL_DBG
                    .get()
                    .unwrap_unchecked()
                    .lock()
                    .write_str(&text)
                    .unwrap();
            }
            SERIAL_SUBSCRIBER.lock().broadcast(Arc::new(text));
            return;
        };

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
    })
}
