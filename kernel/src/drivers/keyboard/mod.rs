use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use crossbeam_queue::ArrayQueue;
use pc_keyboard::{HandleControl, KeyEvent, Keyboard, ScancodeSet1, layouts};
use spin::{Mutex, Once};
use x86_64::structures::idt::InterruptStackFrame;

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
        scheduler::sched,
        thread::ThreadId,
    },
};

pub static KEY_EVENT_BROADCAST: Broadcaster<KeyEvent> = Broadcaster::new();
/// Set to true when a USB HID keyboard is active; suppresses PS/2 keyboard broadcasting.
pub static USB_KEYBOARD_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_POLLERS: BlockingMutex<Vec<(PollKey, Arc<PollEntry>, Arc<Subscriber<KeyEvent>>)>> =
    BlockingMutex::new(Vec::new());
static KEYBOARD_NEXT_POLL_KEY: AtomicU64 = AtomicU64::new(1);

pub(crate) static SCANCODE_QUEUE: Once<ArrayQueue<u8>> = Once::new();
const QUEUE_SIZE: usize = 2048;

pub extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::drivers::ps2_drain_buffer();
    unsafe { get_lapic().end_of_interrupt() };
}

pub static KEYBOARD_THREAD_ID: Once<ThreadId> = Once::new();

/// Early init: create the scancode queue before IRQs are enabled.
pub fn init() {
    SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));
}

pub extern "C" fn driver_main() -> ! {
    // PS/2 hardware init is done by ps2::init_ps2_controller() before
    // this thread is spawned. We only set up the driver thread state here.
    let thread = sched().current_thread().unwrap();
    KEYBOARD_THREAD_ID.call_once(|| thread.id);
    thread.set_priority(10);

    // Layout doesn't matter -- we only use add_byte() for scancode→KeyCode,
    // not process_keyevent() for character decoding. Decoding is done in userspace.
    let mut keyboard = Keyboard::new(
        ScancodeSet1::new(),
        layouts::Us104Key,
        HandleControl::Ignore,
    );

    let queue = SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));
    let device = Arc::new(KeyboardDevice);
    register_device_str("/kbd", device).expect("Error registering device");

    let mut raw_events: Vec<KeyEvent> = Vec::new();
    loop {
        sched().thread_park_while(|| queue.is_empty());

        while let Some(scancode) = queue.pop() {
            if let Ok(Some(event)) = keyboard.add_byte(scancode) {
                raw_events.push(event);
            }
        }

        if !raw_events.is_empty() && !USB_KEYBOARD_ACTIVE.load(Ordering::Relaxed) {
            KEY_EVENT_BROADCAST.broadcast_many(&raw_events);
            notify_keyboard_pollers();
            raw_events.clear();
        } else {
            raw_events.clear();
        }

        KEY_EVENT_BROADCAST.cleanup();
    }
}

#[derive(Debug)]
pub struct KeyboardDevice;

#[derive(Debug)]
struct KeyboardPoll;

fn keyboard_poll_state(subscriber: &Subscriber<KeyEvent>) -> PollState {
    PollState {
        readable: !subscriber.is_empty(),
        writable: false,
        error: false,
        hangup: false,
        invalid: false,
    }
}

fn notify_keyboard_pollers() {
    // Snapshot poller entries under lock, then notify outside to avoid
    // holding BlockingMutex while wake_thread spins (priority inversion).
    let snapshot: heapless::Vec<(Arc<PollEntry>, Arc<Subscriber<KeyEvent>>), 16> = {
        let pollers = KEYBOARD_POLLERS.lock();
        let mut v = heapless::Vec::new();
        for (_, entry, subscriber) in pollers.iter() {
            debug_assert!(
                v.push((entry.clone(), subscriber.clone())).is_ok(),
                "too many keyboard pollers"
            );
        }
        v
    };
    for (entry, subscriber) in &snapshot {
        let state = keyboard_poll_state(subscriber);
        entry.update(state);
    }
}

impl Pollable for KeyboardPoll {
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration {
        let subscriber = KEY_EVENT_BROADCAST.subscribe();
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
        let rx = KEY_EVENT_BROADCAST.subscribe();
        let mut buffer = Vec::new();

        while buffer.len() + 3 < count
            && let Some(event) = rx.try_recv()
        {
            let bytes = convert_key_event(event).to_le_bytes();
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

/// Encode a KeyEvent as u32: keycode in low bits, bit 31 set for release.
fn convert_key_event(event: KeyEvent) -> u32 {
    let code = event.code as u32;
    match event.state {
        pc_keyboard::KeyState::Up => code | 0x8000_0000,
        _ => code,
    }
}
