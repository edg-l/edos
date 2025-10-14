use core::{
    hint::spin_loop,
    sync::atomic::{AtomicU64, Ordering},
};

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use crossbeam_queue::ArrayQueue;
use pc_keyboard::{DecodedKey, HandleControl, Keyboard, ScancodeSet1, layouts};
use spin::{Mutex, Once};
use x86_64::{
    instructions::{interrupts::without_interrupts, port::Port},
    structures::idt::InterruptStackFrame,
};

use crate::{
    apic::get_lapic,
    fs::{
        DevFsDevice, DevFsError, MmapRegion, PollState,
        handle::{PollEntry, PollKey, PollRegistration, Pollable},
        register_device_str,
    },
    memory::mapper::MemoryManager,
    thread::{
        broadcast::{Broadcaster, Subscriber},
        mutex::BlockingMutex,
        scheduler::{WakePriority, sched},
        thread::ThreadId,
    },
};

pub static KEYBOARD_BROADCAST: Broadcaster<DecodedKey> = Broadcaster::new();
static KEYBOARD_POLLERS: BlockingMutex<
    Vec<(PollKey, Arc<PollEntry>, Arc<Subscriber<DecodedKey>>)>,
> = BlockingMutex::new(Vec::new());
static KEYBOARD_NEXT_POLL_KEY: AtomicU64 = AtomicU64::new(1);

static SCANCODE_QUEUE: Once<ArrayQueue<u8>> = Once::new();
const QUEUE_SIZE: usize = 2048;

pub extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use x86_64::instructions::port::Port;

    let mut data_port = Port::new(0x60);
    let mut status_port = Port::new(0x64);
    let queue = SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));

    // Drain all bytes currently buffered by the controller. Limit the loop so the ISR
    // cannot monopolize the CPU if the device misbehaves.
    for _ in 0..8 {
        let status: u8 = unsafe { status_port.read() };
        if status & 0x01 == 0 {
            break;
        }

        let scancode: u8 = unsafe { data_port.read() };
        queue.force_push(scancode);
    }

    if let Some(tid) = KEYBOARD_THREAD_ID.get() {
        sched().wake_thread_irq(*tid, WakePriority::Interrupt);
    }

    unsafe { get_lapic().end_of_interrupt() };
}

pub static KEYBOARD_THREAD_ID: Once<ThreadId> = Once::new();

pub extern "C" fn driver_main() -> ! {
    without_interrupts(|| unsafe {
        enable_ps2_keyboard();

        let thread = sched().current_thread().unwrap();
        KEYBOARD_THREAD_ID.call_once(|| thread.id);
        thread.set_priority(10); // Priority 10, input should be handled asap
    });

    let mut keyboard = Keyboard::new(
        ScancodeSet1::new(),
        layouts::De105Key,
        HandleControl::Ignore,
    );

    let queue = SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));
    let device = Arc::new(KeyboardDevice);
    register_device_str("/kbd", device).expect("Error registering device");

    let mut decoded_events: Vec<DecodedKey> = Vec::new();
    loop {
        while let Some(scancode) = queue.pop() {
            if let Ok(Some(event)) = keyboard.add_byte(scancode)
                && let Some(key_event) = keyboard.process_keyevent(event)
            {
                decoded_events.push(key_event);
            }
        }

        if !decoded_events.is_empty() {
            KEYBOARD_BROADCAST.broadcast_many(&decoded_events);
            notify_keyboard_pollers();
            decoded_events.clear();
        }

        KEYBOARD_BROADCAST.cleanup();
        sched().thread_park();
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

        // Consume the expected ACK (0xFA) and self-test pass (0xAA) bytes so they don't
        // appear as spurious key events for user-space on the first read.
        let _ = read_data_with_timeout(&mut command_port, &mut data_port);
        let _ = read_data_with_timeout(&mut command_port, &mut data_port);
    }
}

unsafe fn read_data_with_timeout(
    command_port: &mut Port<u8>,
    data_port: &mut Port<u8>,
) -> Option<u8> {
    for _ in 0..1_000 {
        if unsafe { command_port.read() } & 0x01 != 0 {
            return Some(unsafe { data_port.read() });
        }
        spin_loop();
    }
    None
}

#[derive(Debug)]
pub struct KeyboardDevice;

#[derive(Debug)]
struct KeyboardPoll;

fn keyboard_poll_state(subscriber: &Subscriber<DecodedKey>) -> PollState {
    PollState {
        readable: !subscriber.is_empty(),
        writable: false,
        error: false,
        hangup: false,
        invalid: false,
    }
}

fn notify_keyboard_pollers() {
    let pollers = KEYBOARD_POLLERS.lock();
    for (_, entry, subscriber) in pollers.iter() {
        let state = keyboard_poll_state(subscriber);
        entry.update(state);
    }
}

impl Pollable for KeyboardPoll {
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration {
        let subscriber = KEYBOARD_BROADCAST.subscribe();
        let state = keyboard_poll_state(&subscriber);
        entry.update(state);

        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = KEYBOARD_NEXT_POLL_KEY.fetch_add(1, Ordering::Relaxed);
            KEYBOARD_POLLERS.lock().push((key, entry, subscriber));
            PollRegistration {
                initial: state,
                key: Some(key),
            }
        }
    }

    fn unregister(&self, key: PollKey) {
        KEYBOARD_POLLERS
            .lock()
            .retain(|(stored, _, _)| *stored != key);
    }
}

impl DevFsDevice for KeyboardDevice {
    fn read(&self, _offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        let rx = KEYBOARD_BROADCAST.subscribe();
        let mut buffer = Vec::new();

        // fill remaining
        while buffer.len() + 3 < count
            && let Some(key) = rx.try_recv()
        {
            let bytes = convert_key(key).to_le_bytes();
            buffer.extend(bytes);
        }

        notify_keyboard_pollers();

        Ok(buffer)
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn ioctl(&self, _request: u64, _arg: u64) -> Result<u64, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn poll(&self) -> Result<Box<dyn Pollable>, DevFsError> {
        Ok(Box::new(KeyboardPoll))
    }

    fn mmap(
        &self,
        _offset: usize,
        _length: usize,
        _memory: Arc<Mutex<MemoryManager>>,
    ) -> Result<MmapRegion, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn size(&self) -> u64 {
        0
    }
}

fn convert_key(key: DecodedKey) -> u32 {
    match key {
        DecodedKey::Unicode(c) => c as u32,
        DecodedKey::RawKey(scancode) => scancode as u32 | 0x80000000, // Set high bit for raw
    }
}
