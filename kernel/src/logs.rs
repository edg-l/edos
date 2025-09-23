use alloc::string::String;
use x86_64::instructions::interrupts::without_interrupts;

use crate::{
    serial::add_serial_log,
    thread::{broadcast::Broadcaster, scheduler::sched},
    timer::uptime_us,
    util::per_cpu::get_percpu_data,
};

pub static LOG_BROADCAST: Broadcaster<String> = Broadcaster::new();

pub fn log(args: core::fmt::Arguments) {
    without_interrupts(|| {
        use core::fmt::Write;
        let mut buf = alloc::string::String::new();
        let uptime_us = uptime_us();
        let secs = uptime_us / 1_000_000;
        let us = uptime_us % 1_000_000;

        let cpu = get_percpu_data();
        let cpu_idx = cpu.lapic_id;

        if !cpu.scheduler.is_null() {
            let sched = sched();

            if let Some(thread) = sched.current_thread() {
                let name = &*thread.name;

                // build prefix
                let _ = write!(
                    buf,
                    "[{secs}.{us:06}] <cpu-{}:{}:{}:{}> ",
                    cpu_idx,
                    name,
                    if thread.user.is_none() { "k" } else { "u" },
                    thread.id.0,
                );

                // append user’s message
                let _ = buf.write_fmt(args);
                buf.push('\n');

                add_serial_log(&buf);
                LOG_BROADCAST.broadcast(buf);
            } else {
                let _ = write!(buf, "[{secs}.{us:06}] <cpu-{}:kernel> ", cpu_idx,);
                let _ = buf.write_fmt(args);
                buf.push('\n');
                add_serial_log(&buf);
            }
        }
    })
}

#[macro_export]
macro_rules! log {
    // default logger
    ($fmt:literal $(, $arg:expr)*) => {
        $crate::logs::log(format_args!($fmt $(, $arg)*))
    };
}

pub fn init() {}
