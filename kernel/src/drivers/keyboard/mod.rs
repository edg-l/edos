use crossbeam_queue::{ArrayQueue, SegQueue};
use pc_keyboard::{DecodedKey, HandleControl, Keyboard, ScancodeSet1, layouts};
use spin::Once;
use x86_64::{
    instructions::{hlt, interrupts::without_interrupts, port::Port},
    structures::idt::InterruptStackFrame,
};

use crate::{apic::get_lapic, serial_println};

static SCANCODE_QUEUE: Once<ArrayQueue<u8>> = Once::new();
const QUEUE_SIZE: usize = 2048;

pub extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use x86_64::instructions::port::Port;

    let mut port = Port::new(0x60);

    let scancode: u8 = unsafe { port.read() };

    let queue = SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));
    queue.force_push(scancode);

    unsafe { get_lapic().end_of_interrupt() };
}

pub fn driver_main() -> ! {
    without_interrupts(|| unsafe {
        enable_ps2_keyboard();
    });

    let mut keyboard = Keyboard::new(
        ScancodeSet1::new(),
        layouts::De105Key,
        HandleControl::Ignore,
    );

    let queue = SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));

    loop {
        while let Some(scancode) = queue.pop() {
            if let Ok(Some(event)) = keyboard.add_byte(scancode)
                && let Some(key_event) = keyboard.process_keyevent(event)
            {
                match key_event {
                    DecodedKey::RawKey(key_code) => {
                        serial_println!("kb: {key_code:?}")
                    }
                    DecodedKey::Unicode(c) => {
                        serial_println!("kb: {c}");
                    }
                }
            }
        }
        hlt();
    }
}

unsafe fn enable_ps2_keyboard() {
    // Disable both PS/2 ports
    let mut command_port = Port::new(0x64);
    let mut data_port = Port::new(0x60);

    unsafe {
        command_port.write(0xAD_u8); // Disable first PS/2 port
        command_port.write(0xA7_u8); // Disable second PS/2 port

        // Flush output buffer
        data_port.read();

        // Set controller configuration
        command_port.write(0x20_u8); // Read configuration byte
        let mut config = data_port.read();
        config |= 0x01; // Enable first port interrupt
        config &= !0x10; // Clear bit 4 (enable first port clock)

        command_port.write(0x60_u8); // Write configuration byte
        data_port.write(config);

        // Enable first PS/2 port
        command_port.write(0xAE_u8);

        // Reset keyboard
        data_port.write(0xFF_u8);
    }
}
