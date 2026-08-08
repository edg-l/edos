use alloc::{string::String, sync::Arc, vec::Vec};
use crossbeam_queue::ArrayQueue;
use spin::{Once, RwLock};

use crate::{
    fs::{DevFsDevice, register_device_str},
    serial::add_serial_log,
    thread::{scheduler::sched, util::queue_spawn_kthread_named, waitqueue::WaitQueue},
    timer::uptime_us,
    util::per_cpu::get_percpu_data,
};

static LOG_QUEUE: Once<ArrayQueue<String>> = Once::new();
static LOG_WAITQ: WaitQueue = WaitQueue::new();

fn get_log_queue() -> &'static ArrayQueue<String> {
    LOG_QUEUE.call_once(|| ArrayQueue::new(1000))
}

pub fn log(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let mut buf = alloc::string::String::new();
    let uptime_us = uptime_us();
    let secs = uptime_us / 1_000_000;
    let us = uptime_us % 1_000_000;

    let cpu = get_percpu_data();
    let cpu_idx = cpu.lapic_id.get();

    if !cpu.scheduler.get().is_null() {
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

            get_log_queue().push(buf).ok();
            LOG_WAITQ.wake_one();
        } else {
            let _ = write!(buf, "[{secs}.{us:06}] <cpu-{}:kernel> ", cpu_idx,);
            let _ = buf.write_fmt(args);
            buf.push('\n');
            add_serial_log(&buf);
        }
    }
}

struct LogDevfs {
    log: RwLock<String>,
}

impl LogDevfs {
    pub fn new() -> Self {
        Self {
            log: Default::default(),
        }
    }

    pub fn add_log(&self, log: String) {
        if log.is_empty() {
            return;
        }
        let mut s = self.log.write();
        s.push_str(&log);
    }
}

impl DevFsDevice for LogDevfs {
    fn read(
        &self,
        offset: usize,
        count: usize,
    ) -> Result<alloc::vec::Vec<u8>, crate::fs::DevFsError> {
        let log = self.log.read();
        let bytes = log.as_bytes();
        let upper = bytes.len().min(offset + count);

        if offset >= log.len() {
            Ok(Vec::new())
        } else {
            Ok(bytes[offset..upper].to_vec())
        }
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, crate::fs::DevFsError> {
        Err(crate::fs::DevFsError::Unsupported)
    }

    fn ioctl(&self, _request: u64, _arg: u64) -> Result<u64, crate::fs::DevFsError> {
        Err(crate::fs::DevFsError::Unsupported)
    }

    fn poll(
        &self,
    ) -> Result<alloc::boxed::Box<dyn crate::fs::handle::Pollable>, crate::fs::DevFsError> {
        Err(crate::fs::DevFsError::Unsupported)
    }

    fn mmap(
        &self,
        _offset: usize,
        _length: usize,
        _memory: alloc::sync::Arc<
            crate::thread::preempt::PreemptSpinlock<crate::memory::mapper::MemoryManager>,
        >,
    ) -> Result<crate::fs::MmapRegion, crate::fs::DevFsError> {
        Err(crate::fs::DevFsError::Unsupported)
    }

    fn size(&self) -> u64 {
        0
    }
}

pub fn init() {
    queue_spawn_kthread_named("klogger", klogger as *const () as u64);
}

pub fn klogger() {
    let queue = LOG_QUEUE.call_once(|| ArrayQueue::new(1000));

    let dev = Arc::new(LogDevfs::new());
    register_device_str("klog", dev.clone()).expect("failed to register klog device");

    loop {
        while let Some(msg) = queue.pop() {
            add_serial_log(&msg);
            dev.add_log(msg);
        }
        let _ = LOG_WAITQ.wait_until(|| !queue.is_empty());
    }
}

#[macro_export]
macro_rules! log {
    // default logger
    ($fmt:literal $(, $arg:expr)*) => {
        $crate::logs::log(format_args!($fmt $(, $arg)*))
    };
}
