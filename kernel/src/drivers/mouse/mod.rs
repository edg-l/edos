//! PS/2 mouse driver with event broadcasting and DevFS interface.

use core::{
    hint::spin_loop,
    sync::atomic::{AtomicI32, AtomicU8, AtomicU64, Ordering},
};

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use crossbeam_queue::ArrayQueue;
use spin::Once;
use x86_64::instructions::port::Port;

use crate::{
    fs::{
        DevFsDevice, DevFsError, MmapRegion, PollState,
        handle::{PollEntry, PollKey, PollRegistration, Pollable},
        register_device_str,
    },
    graphics::api::screen_info,
    log,
    memory::mapper::MemoryManager,
    thread::{
        broadcast::{Broadcaster, Subscriber},
        mutex::BlockingMutex,
        scheduler::{WakePriority, sched},
        thread::ThreadId,
    },
};

// PS/2 ports
const DATA_PORT: u16 = 0x60;
const CMD_PORT: u16 = 0x64;

// Commands to controller (write to 0x64)
const CMD_ENABLE_AUX: u8 = 0xA8; // Enable auxiliary (mouse) port
const CMD_GET_CONFIG: u8 = 0x20; // Read config byte
const CMD_SET_CONFIG: u8 = 0x60; // Write config byte
const CMD_WRITE_AUX: u8 = 0xD4; // Send next byte to mouse

// Commands to mouse (write to 0x60 after CMD_WRITE_AUX)
const MOUSE_SET_DEFAULTS: u8 = 0xF6;
const MOUSE_ENABLE_STREAMING: u8 = 0xF4;
const MOUSE_SET_SAMPLE_RATE: u8 = 0xF3;
const MOUSE_GET_DEVICE_ID: u8 = 0xF2;

// Mouse packet bits
const PACKET_X_SIGN: u8 = 0x10;
const PACKET_Y_SIGN: u8 = 0x20;
const PACKET_X_OVERFLOW: u8 = 0x40;
const PACKET_Y_OVERFLOW: u8 = 0x80;

/// Mouse event containing position and button state
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MouseEvent {
    /// Absolute X position
    pub x: i32,
    /// Absolute Y position
    pub y: i32,
    /// Relative X movement
    pub dx: i16,
    /// Relative Y movement
    pub dy: i16,
    /// Button state (bit 0=left, 1=right, 2=middle)
    pub buttons: u8,
    /// Scroll wheel delta (if supported)
    pub scroll: i8,
    /// Padding for alignment
    _padding: [u8; 2],
}

impl MouseEvent {
    #[allow(dead_code)]
    pub const fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            dx: 0,
            dy: 0,
            buttons: 0,
            scroll: 0,
            _padding: [0; 2],
        }
    }
}

/// Broadcaster for mouse events
pub static MOUSE_BROADCAST: Broadcaster<MouseEvent> = Broadcaster::new();

// Current mouse state
static MOUSE_POSITION: (AtomicI32, AtomicI32) = (AtomicI32::new(0), AtomicI32::new(0));
static MOUSE_BUTTONS: AtomicU8 = AtomicU8::new(0);

// Scancode queue for interrupt handler
static SCANCODE_QUEUE: Once<ArrayQueue<u8>> = Once::new();
const QUEUE_SIZE: usize = 256;

// Screen bounds
static SCREEN_WIDTH: AtomicI32 = AtomicI32::new(800);
static SCREEN_HEIGHT: AtomicI32 = AtomicI32::new(600);

// Whether the mouse has a scroll wheel
static HAS_WHEEL: AtomicU8 = AtomicU8::new(0);

// Driver thread ID for waking
pub static MOUSE_THREAD_ID: Once<ThreadId> = Once::new();

/// Push a mouse byte from an external handler (e.g. keyboard IRQ
/// draining a mouse byte from the shared 8042 buffer).
pub fn push_mouse_byte(byte: u8) {
    if let Some(queue) = SCANCODE_QUEUE.get() {
        let _ = queue.push(byte);
        if let Some(tid) = MOUSE_THREAD_ID.get() {
            sched().wake_thread_irq(*tid, WakePriority::Interrupt);
        }
    }
}

// Polling support
static MOUSE_POLLERS: BlockingMutex<Vec<(PollKey, Arc<PollEntry>, Arc<Subscriber<MouseEvent>>)>> =
    BlockingMutex::new(Vec::new());
static MOUSE_NEXT_POLL_KEY: AtomicU64 = AtomicU64::new(1);

/// Set the screen bounds for mouse position clamping
pub fn set_screen_bounds(width: i32, height: i32) {
    SCREEN_WIDTH.store(width, Ordering::Relaxed);
    SCREEN_HEIGHT.store(height, Ordering::Relaxed);
}

/// Get the current mouse position
pub fn get_position() -> (i32, i32) {
    (
        MOUSE_POSITION.0.load(Ordering::Relaxed),
        MOUSE_POSITION.1.load(Ordering::Relaxed),
    )
}

/// Get the current button state
pub fn get_buttons() -> u8 {
    MOUSE_BUTTONS.load(Ordering::Relaxed)
}

/// Initialize the mouse driver (early init, no thread context required)
pub fn init() {
    // Initialize the scancode queue early so interrupt handler can use it
    SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));
}

/// Called from the IRQ12 interrupt handler
pub fn handle_interrupt() {
    let queue = SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));

    // Read status: bit 0 = data available, bit 5 = aux (mouse) data.
    // Only read if both bits are set — this is mouse data, not keyboard.
    let status: u8 = unsafe { Port::new(CMD_PORT).read() };
    if status & 0x21 == 0x21 {
        let byte: u8 = unsafe { Port::new(DATA_PORT).read() };
        let _ = queue.push(byte);

        // Wake the driver thread
        if let Some(tid) = MOUSE_THREAD_ID.get() {
            sched().wake_thread_irq(*tid, WakePriority::Interrupt);
        }
    }
}

unsafe fn enable_ps2_mouse() {
    let mut command_port = Port::<u8>::new(CMD_PORT);
    let mut data_port = Port::<u8>::new(DATA_PORT);

    unsafe {
        // 1. Enable auxiliary (mouse) port
        wait_write();
        command_port.write(CMD_ENABLE_AUX);

        // 2. Enable interrupts for aux port
        wait_write();
        command_port.write(CMD_GET_CONFIG);
        wait_read();
        let mut config: u8 = data_port.read();
        config |= 0x02; // Enable IRQ12 (aux port interrupt)
        config &= !0x20; // Enable aux port clock

        wait_write();
        command_port.write(CMD_SET_CONFIG);
        wait_write();
        data_port.write(config);
    }

    // 3. Reset mouse to defaults
    mouse_write(MOUSE_SET_DEFAULTS);
    let _ = mouse_read_timeout(); // ACK

    // 4. Try to enable scroll wheel (magic sequence)
    // Set sample rate: 200, 100, 80 to enable wheel
    for rate in [200u8, 100, 80] {
        mouse_write(MOUSE_SET_SAMPLE_RATE);
        let _ = mouse_read_timeout(); // ACK
        mouse_write(rate);
        let _ = mouse_read_timeout(); // ACK
    }

    // Check device ID (3 = wheel mouse, 4 = 5-button)
    mouse_write(MOUSE_GET_DEVICE_ID);
    let _ = mouse_read_timeout(); // ACK
    if let Some(device_id) = mouse_read_timeout() {
        let has_wheel = device_id >= 3;
        HAS_WHEEL.store(has_wheel as u8, Ordering::Relaxed);
        log!("Mouse device ID: {}, has wheel: {}", device_id, has_wheel);
    }

    // 5. Set sample rate to 100 for responsive input
    mouse_write(MOUSE_SET_SAMPLE_RATE);
    let _ = mouse_read_timeout();
    mouse_write(100);
    let _ = mouse_read_timeout();

    // 6. Enable streaming mode
    mouse_write(MOUSE_ENABLE_STREAMING);
    let _ = mouse_read_timeout(); // ACK

    log!("PS/2 mouse initialized");
}

fn wait_write() {
    for _ in 0..10000 {
        if unsafe { Port::<u8>::new(CMD_PORT).read() } & 0x02 == 0 {
            return;
        }
        spin_loop();
    }
}

fn wait_read() {
    for _ in 0..10000 {
        if unsafe { Port::<u8>::new(CMD_PORT).read() } & 0x01 != 0 {
            return;
        }
        spin_loop();
    }
}

fn mouse_write(byte: u8) {
    wait_write();
    unsafe { Port::new(CMD_PORT).write(CMD_WRITE_AUX) };
    wait_write();
    unsafe { Port::new(DATA_PORT).write(byte) };
}

fn mouse_read_timeout() -> Option<u8> {
    for _ in 0..10000 {
        if unsafe { Port::<u8>::new(CMD_PORT).read() } & 0x01 != 0 {
            return Some(unsafe { Port::new(DATA_PORT).read() });
        }
        spin_loop();
    }
    None
}

/// Main driver thread
pub extern "C" fn driver_main() -> ! {
    log!("Initializing PS/2 mouse driver");

    let thread = sched().current_thread().unwrap();
    MOUSE_THREAD_ID.call_once(|| thread.id);
    thread.set_priority(10); // High priority for input

    // Get screen dimensions and center the mouse
    let info = screen_info();
    set_screen_bounds(info.width as i32, info.height as i32);
    MOUSE_POSITION
        .0
        .store(info.width as i32 / 2, Ordering::Relaxed);
    MOUSE_POSITION
        .1
        .store(info.height as i32 / 2, Ordering::Relaxed);

    // Initialize PS/2 mouse hardware
    unsafe {
        enable_ps2_mouse();
    }

    // Register the device
    let device = Arc::new(MouseDevice);
    register_device_str("/mouse", device).expect("failed to register mouse device");
    log!("Mouse device registered at /dev/mouse");

    let has_wheel = HAS_WHEEL.load(Ordering::Relaxed) != 0;
    let packet_size = if has_wheel { 4 } else { 3 };
    let mut packet = [0u8; 4];
    let mut packet_idx = 0;

    let queue = SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));

    loop {
        sched().thread_park_while(|| queue.is_empty());

        while let Some(byte) = queue.pop() {
            // Sync: first byte must have bit 3 set (always-1 bit in PS/2 protocol)
            if packet_idx == 0 && (byte & 0x08) == 0 {
                continue; // Resync
            }

            packet[packet_idx] = byte;
            packet_idx += 1;

            if packet_idx >= packet_size {
                process_packet(&packet[..packet_size]);
                packet_idx = 0;
            }
        }

        MOUSE_BROADCAST.cleanup();
    }
}

fn process_packet(packet: &[u8]) {
    let buttons = packet[0] & 0x07;

    // Extract signed deltas
    let mut dx = packet[1] as i16;
    let mut dy = packet[2] as i16;

    if packet[0] & PACKET_X_SIGN != 0 {
        dx |= !0xFF; // Sign extend
    }
    if packet[0] & PACKET_Y_SIGN != 0 {
        dy |= !0xFF;
    }

    // PS/2 mouse Y is inverted (up is positive in PS/2, we want down positive)
    dy = -dy;

    // Ignore overflow packets
    if packet[0] & (PACKET_X_OVERFLOW | PACKET_Y_OVERFLOW) != 0 {
        return;
    }

    // Update absolute position with clamping
    let max_x = SCREEN_WIDTH.load(Ordering::Relaxed);
    let max_y = SCREEN_HEIGHT.load(Ordering::Relaxed);

    let old_x = MOUSE_POSITION.0.load(Ordering::Relaxed);
    let old_y = MOUSE_POSITION.1.load(Ordering::Relaxed);

    let new_x = (old_x + dx as i32).clamp(0, max_x - 1);
    let new_y = (old_y + dy as i32).clamp(0, max_y - 1);

    MOUSE_POSITION.0.store(new_x, Ordering::Relaxed);
    MOUSE_POSITION.1.store(new_y, Ordering::Relaxed);
    MOUSE_BUTTONS.store(buttons, Ordering::Relaxed);

    // Scroll wheel (4th byte if present)
    let scroll = if packet.len() >= 4 {
        packet[3] as i8
    } else {
        0
    };

    let event = MouseEvent {
        x: new_x,
        y: new_y,
        dx,
        dy,
        buttons,
        scroll,
        _padding: [0; 2],
    };

    MOUSE_BROADCAST.broadcast(event);
    notify_mouse_pollers();
}

fn mouse_poll_state(subscriber: &Subscriber<MouseEvent>) -> PollState {
    PollState {
        readable: !subscriber.is_empty(),
        writable: false,
        error: false,
        hangup: false,
        invalid: false,
    }
}

fn notify_mouse_pollers() {
    // Snapshot poller entries under lock, then notify outside to avoid
    // holding BlockingMutex while wake_thread spins (priority inversion).
    let snapshot: heapless::Vec<(Arc<PollEntry>, Arc<Subscriber<MouseEvent>>), 16> = {
        let pollers = MOUSE_POLLERS.lock();
        let mut v = heapless::Vec::new();
        for (_, entry, subscriber) in pollers.iter() {
            debug_assert!(
                v.push((entry.clone(), subscriber.clone())).is_ok(),
                "too many mouse pollers"
            );
        }
        v
    };
    for (entry, subscriber) in &snapshot {
        let state = mouse_poll_state(subscriber);
        entry.update(state);
    }
}

/// DevFS device for mouse
#[derive(Debug)]
struct MouseDevice;

#[derive(Debug)]
struct MousePoll;

impl Pollable for MousePoll {
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration {
        let subscriber = MOUSE_BROADCAST.subscribe();
        let state = mouse_poll_state(&subscriber);
        entry.update(state);

        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = MOUSE_NEXT_POLL_KEY.fetch_add(1, Ordering::Relaxed);
            MOUSE_POLLERS.lock().push((key, entry, subscriber));
            PollRegistration {
                initial: state,
                key: Some(key),
            }
        }
    }

    fn unregister(&self, key: PollKey) {
        MOUSE_POLLERS.lock().retain(|(stored, _, _)| *stored != key);
    }
}

impl DevFsDevice for MouseDevice {
    fn read(&self, _offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        // Return current state as MouseEvent bytes
        let (x, y) = get_position();
        let buttons = get_buttons();

        let event = MouseEvent {
            x,
            y,
            dx: 0,
            dy: 0,
            buttons,
            scroll: 0,
            _padding: [0; 2],
        };

        let bytes = unsafe {
            core::slice::from_raw_parts(
                &event as *const MouseEvent as *const u8,
                core::mem::size_of::<MouseEvent>(),
            )
        };

        Ok(bytes[..count.min(bytes.len())].to_vec())
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn ioctl(&self, _request: u64, _arg: u64) -> Result<u64, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn poll(&self) -> Result<Box<dyn Pollable>, DevFsError> {
        Ok(Box::new(MousePoll))
    }

    fn mmap(
        &self,
        _offset: usize,
        _length: usize,
        _memory: Arc<spin::Mutex<MemoryManager>>,
    ) -> Result<MmapRegion, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn size(&self) -> u64 {
        core::mem::size_of::<MouseEvent>() as u64
    }
}
