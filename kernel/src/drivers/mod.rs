use core::sync::atomic::{AtomicBool, Ordering};

use crate::{graphics, thread::util::queue_spawn_kthread_named};

pub mod ahci;
pub mod dma;
pub mod fpu;

/// Spinlock for serializing 8042 PS/2 controller data port access.
///
/// IRQ1 (keyboard) and IRQ12 (mouse) can fire simultaneously on different
/// CPUs. Both need to read the status register (0x64) and data port (0x60)
/// on the same 8042 controller. Without serialization, one handler can
/// consume a byte intended for the other, corrupting the PS/2 byte stream.
///
/// Both IRQ handlers call ps2_drain_buffer() which spin-acquires this lock,
/// drains ALL pending bytes (both keyboard and mouse), and routes each byte
/// to the correct queue based on the status register's bit 5.
static PS2_LOCK: AtomicBool = AtomicBool::new(false);

/// Drain all pending bytes from the 8042 PS/2 controller buffer.
/// Routes keyboard bytes to the keyboard queue and mouse bytes to the mouse queue.
/// Called from both IRQ1 (keyboard) and IRQ12 (mouse) handlers.
pub fn ps2_drain_buffer() {
    use x86_64::instructions::port::Port;

    // Spin-acquire: brief spin is acceptable in IRQ context since the
    // critical section is just a few port reads (~microseconds).
    while PS2_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }

    let mut got_keyboard = false;
    let mut got_mouse = false;

    for _ in 0..8 {
        let status: u8 = unsafe { Port::<u8>::new(0x64).read() };
        if status & 0x01 == 0 {
            break; // No data available
        }
        let byte: u8 = unsafe { Port::<u8>::new(0x60).read() };
        if status & 0x20 != 0 {
            // Mouse byte (bit 5 set)
            mouse::push_mouse_byte(byte);
            got_mouse = true;
        } else {
            // Keyboard byte
            if let Some(queue) = keyboard::SCANCODE_QUEUE.get() {
                let _ = queue.push(byte);
                got_keyboard = true;
            }
        }
    }

    PS2_LOCK.store(false, Ordering::Release);

    // Wake driver threads outside the lock
    let sched = crate::thread::scheduler::sched();
    if got_keyboard {
        if let Some(tid) = keyboard::KEYBOARD_THREAD_ID.get() {
            sched.wake_thread_irq(*tid, crate::thread::scheduler::WakePriority::Interrupt);
        }
    }
    if got_mouse {
        if let Some(tid) = mouse::MOUSE_THREAD_ID.get() {
            sched.wake_thread_irq(*tid, crate::thread::scheduler::WakePriority::Interrupt);
        }
    }
}
pub mod hpet;
pub mod keyboard;
pub mod mouse;
pub mod msi;
pub mod pci;
pub mod random;
pub mod rtc;
pub mod tty;
pub mod usb;
pub mod vga;
pub mod virtio;

pub fn init_drivers() {
    hpet::driver::init();
    unsafe { fpu::init_fpu() };
    pci::init(); // pci init is blocking
    ahci::init(); // must be after pci
    usb::init(); // must be after pci
    vga::init();
    graphics::init();
    tty::init();
    random::init();
    // Initialize PS/2 controller and both devices BEFORE spawning driver threads.
    // Both keyboard and mouse share the 8042 controller -- their init sequences
    // must not run concurrently or they clobber each other's config byte writes.
    ps2::init_ps2_controller();
    queue_spawn_kthread_named("keyboard", keyboard::driver_main as *const () as u64);
    queue_spawn_kthread_named("mouse", mouse::driver_main as *const () as u64);
}

/// Combined PS/2 controller initialization for keyboard and mouse.
/// Must run on a single CPU before driver threads are spawned.
mod ps2 {
    use x86_64::instructions::{interrupts::without_interrupts, port::Port};

    use crate::log;

    fn wait_write() {
        for _ in 0..10_000 {
            if unsafe { Port::<u8>::new(0x64).read() } & 0x02 == 0 {
                return;
            }
            core::hint::spin_loop();
        }
    }

    fn wait_read() {
        for _ in 0..10_000 {
            if unsafe { Port::<u8>::new(0x64).read() } & 0x01 != 0 {
                return;
            }
            core::hint::spin_loop();
        }
    }

    fn read_data_timeout() -> Option<u8> {
        for _ in 0..10_000 {
            if unsafe { Port::<u8>::new(0x64).read() } & 0x01 != 0 {
                return Some(unsafe { Port::new(0x60).read() });
            }
            core::hint::spin_loop();
        }
        None
    }

    fn mouse_write(byte: u8) {
        wait_write();
        unsafe { Port::new(0x64).write(0xD4_u8) }; // CMD_WRITE_AUX
        wait_write();
        unsafe { Port::new(0x60).write(byte) };
    }

    /// Initialize the 8042 PS/2 controller, keyboard, and mouse in one
    /// serialized sequence. This avoids the race where concurrent keyboard
    /// and mouse init clobber each other's config byte writes.
    pub fn init_ps2_controller() {
        without_interrupts(|| unsafe {
            let mut cmd: Port<u8> = Port::new(0x64);
            let mut data: Port<u8> = Port::new(0x60);

            // 1. Disable both ports while we configure
            wait_write();
            cmd.write(0xAD); // Disable first port (keyboard)
            wait_write();
            cmd.write(0xA7); // Disable second port (mouse)

            // 2. Flush any pending data
            while Port::<u8>::new(0x64).read() & 0x01 != 0 {
                let _: u8 = data.read();
            }

            // 3. Read config byte, enable IRQs for both ports
            wait_write();
            cmd.write(0x20); // Read config
            wait_read();
            let mut config: u8 = data.read();
            config |= 0x01; // Bit 0: enable first port (keyboard) IRQ
            config |= 0x02; // Bit 1: enable second port (mouse) IRQ
            config &= !0x10; // Bit 4: enable first port clock
            config &= !0x20; // Bit 5: enable second port clock

            wait_write();
            cmd.write(0x60); // Write config
            wait_write();
            data.write(config);

            // 4. Enable both ports
            wait_write();
            cmd.write(0xAE); // Enable first port (keyboard)
            wait_write();
            cmd.write(0xA8); // Enable second port (mouse)

            // 5. Reset keyboard
            wait_write();
            data.write(0xFF);
            let _ = read_data_timeout(); // ACK (0xFA)
            let _ = read_data_timeout(); // Self-test (0xAA)

            // 6. Initialize mouse
            // Reset to defaults
            mouse_write(0xF6); // SET_DEFAULTS
            let _ = read_data_timeout(); // ACK

            // Try to enable scroll wheel (magic sequence: sample rate 200, 100, 80)
            for rate in [200u8, 100, 80] {
                mouse_write(0xF3); // SET_SAMPLE_RATE
                let _ = read_data_timeout();
                mouse_write(rate);
                let _ = read_data_timeout();
            }

            // Check device ID
            mouse_write(0xF2); // GET_DEVICE_ID
            let _ = read_data_timeout(); // ACK
            let has_wheel = if let Some(id) = read_data_timeout() {
                log!("PS/2 mouse device ID: {}, wheel: {}", id, id >= 3);
                id >= 3
            } else {
                false
            };
            super::mouse::HAS_WHEEL.store(has_wheel as u8, core::sync::atomic::Ordering::Relaxed);

            // Set sample rate to 100
            mouse_write(0xF3);
            let _ = read_data_timeout();
            mouse_write(100);
            let _ = read_data_timeout();

            // Enable streaming
            mouse_write(0xF4); // ENABLE_STREAMING
            let _ = read_data_timeout(); // ACK

            log!("PS/2 controller initialized (keyboard + mouse)");
        });

        // Initialize scancode queues early so IRQ handlers can use them
        super::keyboard::init();
        super::mouse::init();
    }
}
