use core::{fmt::Write, time::Duration};

use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use x86_64::instructions::interrupts::without_interrupts;

use crate::{
    serial::add_serial_log,
    thread::{
        broadcast::{LockedBroadcast, new_broadcast},
        scheduler::sched,
        util::queue_spawn_kthread_named,
    },
    timer::uptime_us,
};

pub static LOG_BROADCAST: LockedBroadcast<String> = new_broadcast(1024, true);

#[derive(Debug, Clone)]
pub struct ThreadLogger {
    pub kernel: bool,
    pub id: u64,
    pub name: Arc<Option<String>>,
}

impl Write for ThreadLogger {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let uptime_us = uptime_us();
        let secs = uptime_us / 1_000_000;
        let us = uptime_us % 1_000_000;

        let name = &*self.name;
        let name = name.as_ref().map(|x| (*x).clone()).unwrap_or_else(|| {
            if self.kernel {
                "unk0".to_string()
            } else {
                "unk3".to_string()
            }
        });
        let cpu_idx = sched().lapic_id;

        let text = alloc::format!(
            "[{secs}.{us:06}] <cpu-{}:{}:{}:{}> {s}",
            cpu_idx,
            name,
            if self.kernel { "k" } else { "u" },
            self.id
        );
        LOG_BROADCAST.lock().broadcast(text);
        Ok(())
    }
}

impl ThreadLogger {
    pub fn log(&self, args: core::fmt::Arguments) {
        use core::fmt::Write;
        let mut buf = alloc::string::String::new();
        let uptime_us = uptime_us();
        let secs = uptime_us / 1_000_000;
        let us = uptime_us % 1_000_000;

        let name = &*self.name;
        let name = name.as_ref().map(|x| (*x).clone()).unwrap_or_else(|| {
            if self.kernel {
                "unk0".to_string()
            } else {
                "unk3".to_string()
            }
        });
        let cpu_idx = sched().lapic_id;

        // build prefix
        let _ = write!(
            buf,
            "[{secs}.{us:06}] <cpu-{}:{}:{}:{}> ",
            cpu_idx,
            name,
            if self.kernel { "k" } else { "u" },
            self.id,
        );

        // append user’s message
        let _ = buf.write_fmt(args);
        buf.push('\n');

        LOG_BROADCAST.lock().broadcast(buf);
    }
}

#[macro_export]
macro_rules! log {
    // explicit logger by identifier
    ($logger:ident, $fmt:literal $(, $arg:expr)*) => {
        $logger.log(format_args!($fmt $(, $arg)*))
    };
    // explicit logger by arbitrary expression (use @)
    (@$logger:expr, $($arg:expr)*) => {
        $logger.log(format_args!($($arg)*))
    };
    // default logger
    ($fmt:literal $(, $arg:expr)*) => {
        $crate::thread::scheduler::sched()
            .get_logger()
            .log(format_args!($fmt $(, $arg)*))
    };
}

pub fn init() {
    queue_spawn_kthread_named("logger", thread_log_to_serial as u64);
}

pub fn thread_log_to_serial() -> ! {
    let rx = LOG_BROADCAST.lock().subscribe_or_get();

    loop {
        if let Ok(msg) = rx.recv_timeout(Duration::from_millis(50)) {
            without_interrupts(|| {
                add_serial_log(&msg);
            });
        }
    }
}
