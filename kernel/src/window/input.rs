//! Input event routing for the window server.

use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use crossbeam_queue::ArrayQueue;
use pc_keyboard::DecodedKey;
use spin::{Once, RwLock};

use crate::{
    drivers::{
        keyboard::KEYBOARD_BROADCAST,
        mouse::{MouseEvent, MOUSE_BROADCAST},
    },
    log,
    thread::{
        broadcast::Subscriber,
        scheduler::{WakePriority, sched},
        thread::ThreadId,
        util::queue_spawn_kthread_named,
    },
};

use super::registry::{WINDOW_REGISTRY, WindowId, decoration};

/// Maximum number of queued events per window.
const EVENT_QUEUE_SIZE: usize = 256;

/// Window event types.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub enum WindowEventType {
    /// Mouse moved within the window.
    MouseMove = 1,
    /// Mouse button pressed or released.
    MouseButton = 2,
    /// Mouse scroll wheel.
    MouseScroll = 3,
    /// Key pressed (raw scancode).
    KeyPress = 4,
    /// Key released (raw scancode).
    KeyRelease = 5,
    /// Character typed (Unicode).
    Character = 6,
    /// Window gained focus.
    FocusGained = 7,
    /// Window lost focus.
    FocusLost = 8,
    /// Close was requested.
    CloseRequested = 9,
    /// Window was resized.
    Resize = 10,
}

/// A window event.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct WindowEvent {
    /// Event type.
    pub event_type: u32,
    /// X coordinate (for mouse events, relative to window).
    pub x: i32,
    /// Y coordinate (for mouse events, relative to window).
    pub y: i32,
    /// Key/button code or character.
    pub code: u32,
    /// Additional data (e.g., pressed state for buttons).
    pub data: u32,
}

impl WindowEvent {
    /// Create a mouse move event.
    pub fn mouse_move(x: i32, y: i32) -> Self {
        Self {
            event_type: WindowEventType::MouseMove as u32,
            x,
            y,
            code: 0,
            data: 0,
        }
    }

    /// Create a mouse button event.
    pub fn mouse_button(x: i32, y: i32, button: u8, pressed: bool) -> Self {
        Self {
            event_type: WindowEventType::MouseButton as u32,
            x,
            y,
            code: button as u32,
            data: pressed as u32,
        }
    }

    /// Create a mouse scroll event.
    pub fn mouse_scroll(x: i32, y: i32, delta: i8) -> Self {
        Self {
            event_type: WindowEventType::MouseScroll as u32,
            x,
            y,
            code: 0,
            data: delta as i8 as i32 as u32,
        }
    }

    /// Create a key press event.
    pub fn key_press(key: u32) -> Self {
        Self {
            event_type: WindowEventType::KeyPress as u32,
            x: 0,
            y: 0,
            code: key,
            data: 0,
        }
    }

    /// Create a key release event.
    pub fn key_release(key: u32) -> Self {
        Self {
            event_type: WindowEventType::KeyRelease as u32,
            x: 0,
            y: 0,
            code: key,
            data: 0,
        }
    }

    /// Create a character event.
    pub fn character(ch: char) -> Self {
        Self {
            event_type: WindowEventType::Character as u32,
            x: 0,
            y: 0,
            code: ch as u32,
            data: 0,
        }
    }

    /// Create a focus gained event.
    pub fn focus_gained() -> Self {
        Self {
            event_type: WindowEventType::FocusGained as u32,
            x: 0,
            y: 0,
            code: 0,
            data: 0,
        }
    }

    /// Create a focus lost event.
    pub fn focus_lost() -> Self {
        Self {
            event_type: WindowEventType::FocusLost as u32,
            x: 0,
            y: 0,
            code: 0,
            data: 0,
        }
    }

    /// Create a close requested event.
    pub fn close_requested() -> Self {
        Self {
            event_type: WindowEventType::CloseRequested as u32,
            x: 0,
            y: 0,
            code: 0,
            data: 0,
        }
    }

    /// Create a resize event.
    pub fn resize(width: u32, height: u32) -> Self {
        Self {
            event_type: WindowEventType::Resize as u32,
            x: width as i32,
            y: height as i32,
            code: 0,
            data: 0,
        }
    }
}

/// Per-window event queue.
pub struct WindowEventQueue {
    queue: ArrayQueue<WindowEvent>,
}

impl WindowEventQueue {
    /// Create a new event queue.
    pub fn new() -> Self {
        Self {
            queue: ArrayQueue::new(EVENT_QUEUE_SIZE),
        }
    }

    /// Push an event to the queue.
    pub fn push(&self, event: WindowEvent) -> bool {
        self.queue.push(event).is_ok()
    }

    /// Pop an event from the queue.
    pub fn pop(&self) -> Option<WindowEvent> {
        self.queue.pop()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Get the number of events in the queue.
    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

/// Global storage for window event queues.
pub static WINDOW_EVENTS: RwLock<BTreeMap<WindowId, Arc<WindowEventQueue>>> =
    RwLock::new(BTreeMap::new());

/// Thread ID of the input routing thread.
pub static INPUT_THREAD_ID: Once<ThreadId> = Once::new();

/// Last mouse button state for detecting changes.
static LAST_MOUSE_BUTTONS: spin::Mutex<u8> = spin::Mutex::new(0);

/// Initialize the input routing system.
pub fn init_input_routing() {
    queue_spawn_kthread_named("window-input", input_routing_thread as *const () as u64);
}

/// Get or create an event queue for a window.
pub fn get_or_create_event_queue(window_id: WindowId) -> Arc<WindowEventQueue> {
    {
        let queues = WINDOW_EVENTS.read();
        if let Some(queue) = queues.get(&window_id) {
            return queue.clone();
        }
    }

    let queue = Arc::new(WindowEventQueue::new());
    WINDOW_EVENTS.write().insert(window_id, queue.clone());
    queue
}

/// Remove the event queue for a window.
pub fn remove_event_queue(window_id: WindowId) {
    WINDOW_EVENTS.write().remove(&window_id);
}

/// Poll events for a window, returns up to `max` events.
pub fn poll_events(window_id: WindowId, max: usize) -> Vec<WindowEvent> {
    let queues = WINDOW_EVENTS.read();
    if let Some(queue) = queues.get(&window_id) {
        let mut events = Vec::with_capacity(max.min(EVENT_QUEUE_SIZE));
        while events.len() < max {
            if let Some(event) = queue.pop() {
                events.push(event);
            } else {
                break;
            }
        }
        events
    } else {
        Vec::new()
    }
}

/// Send an event to a specific window.
pub fn send_event(window_id: WindowId, event: WindowEvent) {
    let queues = WINDOW_EVENTS.read();
    if let Some(queue) = queues.get(&window_id) {
        let _ = queue.push(event);
    }
}

/// Main input routing thread.
extern "C" fn input_routing_thread() -> ! {
    log!("Window input routing thread started");

    let thread = sched().current_thread().unwrap();
    INPUT_THREAD_ID.call_once(|| thread.id);
    thread.set_priority(10); // High priority for input

    // Subscribe to input broadcasts
    let mouse_sub: Arc<Subscriber<MouseEvent>> = MOUSE_BROADCAST.subscribe();
    let keyboard_sub: Arc<Subscriber<DecodedKey>> = KEYBOARD_BROADCAST.subscribe();

    loop {
        // Process mouse events
        while let Some(mouse_event) = mouse_sub.try_recv() {
            handle_mouse_event(mouse_event);
        }

        // Process keyboard events
        while let Some(key_event) = keyboard_sub.try_recv() {
            handle_keyboard_event(key_event);
        }

        // Park until woken by new input
        sched().thread_park();
    }
}

/// Handle a mouse event and route to appropriate window.
fn handle_mouse_event(event: MouseEvent) {
    let registry = WINDOW_REGISTRY.read();

    // Find window whose CLIENT area contains the cursor (excludes decorations)
    let window_under_cursor = registry.window_at_client(event.x, event.y);

    // Get currently focused window
    let focused = registry.focused_window();

    // Track button state changes
    let mut last_buttons = LAST_MOUSE_BUTTONS.lock();
    let buttons_changed = event.buttons != *last_buttons;
    let button_pressed = event.buttons & !*last_buttons;
    let button_released = !event.buttons & *last_buttons;
    *last_buttons = event.buttons;
    drop(last_buttons);

    // Handle focus change on mouse button press
    if button_pressed != 0 {
        if let Some(target_window) = window_under_cursor {
            if focused != Some(target_window) {
                // Need to drop read lock before taking write lock
                drop(registry);

                // Send focus lost to old window
                if let Some(old_focused) = focused {
                    send_event(old_focused, WindowEvent::focus_lost());
                }

                // Change focus
                WINDOW_REGISTRY.write().set_focused(target_window);

                // Send focus gained to new window
                send_event(target_window, WindowEvent::focus_gained());

                // Re-acquire read lock
                let registry = WINDOW_REGISTRY.read();

                // Send the click event to the new focused window
                if let Some(window) = registry.get_window(target_window) {
                    // Calculate coordinates relative to client area (excluding decorations)
                    let local_x = event.x - window.x - decoration::BORDER_WIDTH;
                    let local_y = event.y - window.y - decoration::TITLE_HEIGHT;

                    for bit in 0..3 {
                        if button_pressed & (1 << bit) != 0 {
                            send_event(
                                target_window,
                                WindowEvent::mouse_button(local_x, local_y, bit, true),
                            );
                        }
                    }
                }
                return;
            }
        }
    }

    // Route mouse events to window under cursor (for move/scroll) or focused window (for buttons)
    if let Some(target) = window_under_cursor {
        if let Some(window) = registry.get_window(target) {
            // Calculate coordinates relative to client area (excluding decorations)
            let local_x = event.x - window.x - decoration::BORDER_WIDTH;
            let local_y = event.y - window.y - decoration::TITLE_HEIGHT;

            // Always send move events if there's movement
            if event.dx != 0 || event.dy != 0 {
                send_event(target, WindowEvent::mouse_move(local_x, local_y));
            }

            // Send scroll events
            if event.scroll != 0 {
                send_event(target, WindowEvent::mouse_scroll(local_x, local_y, event.scroll));
            }

            // Send button events
            if buttons_changed {
                for bit in 0..3 {
                    if button_pressed & (1 << bit) != 0 {
                        send_event(target, WindowEvent::mouse_button(local_x, local_y, bit, true));
                    }
                    if button_released & (1 << bit) != 0 {
                        send_event(target, WindowEvent::mouse_button(local_x, local_y, bit, false));
                    }
                }
            }
        }
    }
}

/// Handle a keyboard event and route to focused window.
fn handle_keyboard_event(key: DecodedKey) {
    let registry = WINDOW_REGISTRY.read();

    if let Some(focused_id) = registry.focused_window() {
        match key {
            DecodedKey::Unicode(ch) => {
                send_event(focused_id, WindowEvent::character(ch));
            }
            DecodedKey::RawKey(scancode) => {
                // Raw scancodes have bit 7 set for release
                let code = scancode as u32;
                // For now, treat all raw keys as presses
                // In the future, we could track key state for proper press/release
                send_event(focused_id, WindowEvent::key_press(code));
            }
        }
    }
}
