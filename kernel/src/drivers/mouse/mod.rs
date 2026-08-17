//! PS/2 mouse driver with event broadcasting and DevFS interface.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, Ordering};

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use crossbeam_queue::ArrayQueue;
use spin::Once;

use crate::ranked_lock;
use crate::thread::scheduler::{current_thread, thread_park_while};
use crate::{
    debug::lock_order::RANK_DEVICE_POLLERS,
    fs::{
        DevFsDevice, DevFsError, PollState,
        handle::{PollKey, PollRef, PollRegistration, Pollable},
        register_device_str,
    },
    graphics::DISPLAY,
    log,
    thread::{
        broadcast::{Broadcaster, Subscriber},
        mutex::BlockingMutex,
        scheduler::{WakePriority, sched},
        thread::Thread,
    },
};

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
/// Set to true when a USB HID mouse is active; suppresses PS/2 mouse broadcasting.
pub static USB_MOUSE_ACTIVE: AtomicBool = AtomicBool::new(false);

// Current mouse state
static MOUSE_POSITION: (AtomicI32, AtomicI32) = (AtomicI32::new(0), AtomicI32::new(0));
static MOUSE_BUTTONS: AtomicU8 = AtomicU8::new(0);

// Scancode queue for interrupt handler
static SCANCODE_QUEUE: Once<ArrayQueue<u8>> = Once::new();
const QUEUE_SIZE: usize = 256;

// Screen bounds
static SCREEN_WIDTH: AtomicI32 = AtomicI32::new(800);
static SCREEN_HEIGHT: AtomicI32 = AtomicI32::new(600);

// Whether the mouse has a scroll wheel (set by ps2::init_ps2_controller)
pub(crate) static HAS_WHEEL: AtomicU8 = AtomicU8::new(0);

// Driver thread handle for waking from IRQ context.
pub static MOUSE_THREAD_ID: Once<Weak<Thread>> = Once::new();

/// Push a mouse byte from an external handler (e.g. keyboard IRQ
/// draining a mouse byte from the shared 8042 buffer).
pub fn push_mouse_byte(byte: u8) {
    if let Some(queue) = SCANCODE_QUEUE.get() {
        let _ = queue.push(byte);
        if let Some(handle) = MOUSE_THREAD_ID.get() {
            sched().wake_thread_irq(handle, WakePriority::Interrupt);
        }
    }
}

// Polling support
type MousePoller = (PollKey, PollRef, Arc<Subscriber<MouseEvent>>);
static MOUSE_POLLERS: BlockingMutex<Vec<MousePoller>> = BlockingMutex::new(Vec::new());
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

/// Update the global mouse position and button state with relative deltas.
///
/// Clamps the resulting position to the screen bounds.  Returns a `MouseEvent`
/// with the new absolute position and the supplied deltas / button state.
///
/// Called by external drivers (e.g. USB HID) that produce relative motion.
pub fn apply_relative_move(dx: i16, dy: i16, buttons: u8, scroll: i8) -> MouseEvent {
    let max_x = SCREEN_WIDTH.load(Ordering::Relaxed);
    let max_y = SCREEN_HEIGHT.load(Ordering::Relaxed);

    let old_x = MOUSE_POSITION.0.load(Ordering::Relaxed);
    let old_y = MOUSE_POSITION.1.load(Ordering::Relaxed);

    let new_x = (old_x + dx as i32).clamp(0, max_x - 1);
    let new_y = (old_y + dy as i32).clamp(0, max_y - 1);

    MOUSE_POSITION.0.store(new_x, Ordering::Relaxed);
    MOUSE_POSITION.1.store(new_y, Ordering::Relaxed);
    MOUSE_BUTTONS.store(buttons, Ordering::Relaxed);

    MouseEvent {
        x: new_x,
        y: new_y,
        dx,
        dy,
        buttons,
        scroll,
        _padding: [0; 2],
    }
}

/// Update the global mouse position from a device that reports where the
/// pointer *is* rather than how far it moved.
///
/// The deltas in the event are computed rather than reported, since a consumer
/// that only wants to know whether the pointer moved should not have to care
/// which kind of device produced it.
pub fn apply_absolute_move(x: i32, y: i32, buttons: u8, scroll: i8) -> MouseEvent {
    let max_x = SCREEN_WIDTH.load(Ordering::Relaxed);
    let max_y = SCREEN_HEIGHT.load(Ordering::Relaxed);

    let new_x = x.clamp(0, max_x - 1);
    let new_y = y.clamp(0, max_y - 1);

    let old_x = MOUSE_POSITION.0.swap(new_x, Ordering::Relaxed);
    let old_y = MOUSE_POSITION.1.swap(new_y, Ordering::Relaxed);
    MOUSE_BUTTONS.store(buttons, Ordering::Relaxed);

    MouseEvent {
        x: new_x,
        y: new_y,
        dx: (new_x - old_x).clamp(-32768, 32767) as i16,
        dy: (new_y - old_y).clamp(-32768, 32767) as i16,
        buttons,
        scroll,
        _padding: [0; 2],
    }
}

/// The screen rectangle a pointer is clamped to, which is also what an
/// absolute device's logical range is scaled onto.
pub fn screen_size() -> (i32, i32) {
    (
        SCREEN_WIDTH.load(Ordering::Relaxed),
        SCREEN_HEIGHT.load(Ordering::Relaxed),
    )
}

/// Initialize the mouse driver (early init, no thread context required)
pub fn init() {
    // Initialize the scancode queue early so interrupt handler can use it
    SCANCODE_QUEUE.call_once(|| ArrayQueue::new(QUEUE_SIZE));
}

// IRQ12 handling is now done by ps2_drain_buffer() in drivers/mod.rs.

/// Main driver thread
pub extern "C" fn driver_main() -> ! {
    // PS/2 hardware init is done by ps2::init_ps2_controller() before
    // this thread is spawned. We only set up the driver thread state here.
    log!("Mouse driver thread started");

    let thread = current_thread().unwrap();
    MOUSE_THREAD_ID.call_once(|| Arc::downgrade(&thread));
    thread.set_priority(10);

    // Get screen dimensions and center the mouse
    let info = DISPLAY.get().unwrap().lock().screen_info();
    set_screen_bounds(info.width as i32, info.height as i32);
    MOUSE_POSITION
        .0
        .store(info.width as i32 / 2, Ordering::Relaxed);
    MOUSE_POSITION
        .1
        .store(info.height as i32 / 2, Ordering::Relaxed);

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
        thread_park_while(|| queue.is_empty());

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

    if !USB_MOUSE_ACTIVE.load(Ordering::Relaxed) {
        dispatch_mouse_event(event);
    }
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

/// Deliver a mouse event to subscribers and to anyone polling `/dev/mouse`.
///
/// The two halves belong together: pushing onto the subscriber queues without
/// updating the poll entries leaves a `poll()` on `/dev/mouse` blocked
/// forever. Every producer — PS/2 here, USB HID boot reports in
/// `drivers::usb::hid` — goes through this.
pub(crate) fn dispatch_mouse_event(event: MouseEvent) {
    // Before the broadcast, which takes locks and wakes threads: the plane is
    // what the eye sees, and nothing below needs to run first for it to be
    // correct.
    move_tracked_cursor(event.x, event.y);
    MOUSE_BROADCAST.broadcast(event);
    notify_mouse_pollers();
}

/// Put the display's cursor plane where the pointer now is, when the window
/// manager has asked the display to track it.
///
/// This is why a pointer can be current rather than one compositor frame old:
/// the position reaches the plane on the report that produced it. A consumer
/// that samples `/dev/mouse` per frame still sees whatever the last report
/// left, so this changes when the *screen* learns, not what anything reads.
///
/// Never waits. `DISPLAY` is also held across a full-screen blit, and a cursor
/// position is superseded by the next report rather than owed to anyone — so a
/// skipped move costs a millisecond of staleness, while a blocked input thread
/// would delay every report queued behind it and cost exactly the latency this
/// exists to remove.
fn move_tracked_cursor(x: i32, y: i32) {
    if !crate::graphics::CURSOR_TRACKS_POINTER.load(Ordering::Relaxed) {
        return;
    }
    if let Some(display) = DISPLAY.get()
        && let Some(mut display) = display.try_lock()
    {
        display.move_cursor(x.max(0) as u32, y.max(0) as u32);
    }
}

fn notify_mouse_pollers() {
    // Snapshot poller entries under lock, then notify outside to avoid
    // holding BlockingMutex while wake_thread spins (priority inversion).
    let snapshot: heapless::Vec<(PollRef, Arc<Subscriber<MouseEvent>>), 16> = {
        let pollers = ranked_lock!(RANK_DEVICE_POLLERS, "mouse::notify_pollers", MOUSE_POLLERS);
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
    fn register(&self, entry: PollRef) -> PollRegistration {
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
            ranked_lock!(RANK_DEVICE_POLLERS, "mouse::poll_register", MOUSE_POLLERS)
                .push((key, entry, subscriber));
            PollRegistration {
                initial: state,
                key: Some(key),
            }
        }
    }

    fn unregister(&self, key: PollKey) {
        ranked_lock!(RANK_DEVICE_POLLERS, "mouse::poll_unregister", MOUSE_POLLERS)
            .retain(|(stored, _, _)| *stored != key);
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

    fn poll(&self) -> Result<Box<dyn Pollable>, DevFsError> {
        Ok(Box::new(MousePoll))
    }

    fn size(&self) -> u64 {
        core::mem::size_of::<MouseEvent>() as u64
    }
}
