use core::{fmt::Write, time::Duration};

use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use x86_64::instructions::interrupts::without_interrupts;

use crate::{
    acpi::current_cpu_index,
    serial::add_serial_log,
    thread::{
        broadcast::{LockedBroadcast, new_broadcast},
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
        let text = alloc::format!(
            "[{secs}.{us:06}] <cpu-{}:{}:{}:{}> {s}",
            current_cpu_index(),
            name,
            if self.kernel { "k" } else { "u" },
            self.id
        );
        LOG_BROADCAST.lock().broadcast(text);
        Ok(())
    }
}

impl ThreadLogger {
    pub fn log(&self, text: &str) {
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
        let text = alloc::format!(
            "[{secs}.{us:06}] <cpu-{}:{}:{}:{}> {}",
            current_cpu_index(),
            name,
            if self.kernel { "k" } else { "u" },
            self.id,
            text
        );
        LOG_BROADCAST.lock().broadcast(text);
    }
}

#[macro_export]
macro_rules! log {
    ($logger:tt, $($arg:tt)*) => ($logger.log(&alloc::format!("{}\n", format_args!($($arg)*))));
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
