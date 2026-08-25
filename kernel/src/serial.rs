use core::fmt::{self, Write};

use spin::Once;
use uart_16550::{Config, Uart16550Tty, backend::PioBackend};

use crate::{thread::irqlock::IrqSpinlock, timer::uptime_us, util::per_cpu::get_percpu_data};

static SERIAL_DBG: Once<IrqSpinlock<Uart16550Tty<PioBackend>>> = Once::new();

pub fn init() {
    SERIAL_DBG.call_once(|| {
        // SAFETY: 0x3F8 is the standard COM1 base; the kernel is the only user
        // of it, for the whole lifetime of the device.
        let port =
            unsafe { Uart16550Tty::new_port(0x3F8, Config::default()) }.expect("COM1 init failed");
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

/// How long an emergency writer tries for the serial lock before giving up and
/// printing without it.
///
/// It has to exceed one maximal write by the ordinary path, because that is
/// what it waits behind: `_emergency_print` formats into 512 bytes, and 512
/// bytes at the 115200 baud this port is configured for is about 44 ms. Below
/// that the wait expires mid-line and buys nothing — measured, a 10_000-spin
/// bound recovered one page fault in eight where this recovers all eight.
///
/// It is a bound and not a wait, which is the other half of the design: this
/// runs where another CPU may be wedged holding the lock, or where *this* CPU
/// already holds it and faulted inside the serial writer. A shredded line
/// beats a crash path that hangs, so the wait expires and writes anyway, which
/// is what this function did unconditionally before.
const EMERGENCY_SPIN_LIMIT: u32 = 20_000_000;

/// Emergency serial output for crash paths, which takes the serial lock only if
/// it can get it. Use from double-fault, page-fault-kill and panic handlers,
/// where the lock may already be held. Writes the 0x3F8 UART data port directly.
///
/// Taking the lock at all is new: without it the byte loop interleaved with
/// whatever else was writing, and a page fault during a busy `guest-check` run
/// reached the serial log as a line shredded character-by-character with the
/// klog drain's, carrying neither message. Three faults in one run were logged
/// as one, which is how a CI failure went a week without being read.
pub fn emergency_write(msg: &[u8]) {
    let mut spins = 0;
    let _guard = loop {
        if let Some(port) = SERIAL_DBG.get().and_then(|s| s.try_lock()) {
            break Some(port);
        }
        spins += 1;
        if spins >= EMERGENCY_SPIN_LIMIT {
            break None;
        }
        core::hint::spin_loop();
    };

    // Written through the port directly rather than the guard's writer: the
    // point of this path is that it works when the driver's state does not.
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
