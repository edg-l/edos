//! Input event routing for the window server.

use crate::debug::lock_order::{RANK_MOUSE_BUTTONS, RANK_WINDOW_EVENTS, RANK_WINDOW_REGISTRY};
use crate::{ranked_lock, ranked_read, ranked_write};
use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use crossbeam_queue::ArrayQueue;
use pc_keyboard::{KeyEvent, KeyState};

use crate::{
    drivers::{
        keyboard::KEY_EVENT_BROADCAST,
        mouse::{MOUSE_BROADCAST, MouseEvent},
    },
    log,
    thread::{
        broadcast::Subscriber,
        preempt::{PreemptRwLock, PreemptSpinlock},
        util::queue_spawn_kthread_named,
        waitqueue::WaitQueue,
    },
};

use super::registry::{ReadSite, WINDOW_REGISTRY, WindowId, read_tracked};
use crate::thread::scheduler::{current_thread, thread_park_while};

/// Maximum number of queued events per window.
const EVENT_QUEUE_SIZE: usize = 256;

// The event type and the event struct are the client's too, so neither side
// owns them: they live in `window-abi`. Re-exported so the rest of the kernel
// still writes `input::WindowEvent`.
pub use window_abi::{WindowEvent, WindowEventType};

/// Presented frames, counted since boot.
///
/// A client waiting for work compares this against the value it last acted on,
/// which is what lets "a frame was presented" be a wake-up reason without the
/// kernel tracking a subscription per window.
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// Note that the compositor has put a frame on the display, and wake everyone
/// waiting for one.
pub fn present_frame() {
    FRAME_SEQ.fetch_add(1, Ordering::Release);
    // Snapshot the queues and wake outside the lock: waking takes the
    // scheduler's, and the two must not be co-held.
    let waiters: Vec<Arc<WindowEventQueue>> = {
        let queues = ranked_read!(RANK_WINDOW_EVENTS, "window::present_frame", WINDOW_EVENTS);
        queues.values().cloned().collect()
    };
    for queue in waiters {
        queue.waiters.wake_all();
    }
}

/// How many frames have been presented since boot.
pub fn frame_seq() -> u64 {
    FRAME_SEQ.load(Ordering::Acquire)
}

/// Per-window event queue.
pub struct WindowEventQueue {
    queue: ArrayQueue<WindowEvent>,
    /// Threads blocked in `sys_window_wait` on this window.
    ///
    /// A client that sleeps on a timer either wakes with nothing to do or
    /// leaves an event sitting for the rest of its interval. Parking here
    /// instead means the wake and the reason for it are the same event.
    pub waiters: WaitQueue,
}

impl WindowEventQueue {
    /// Create a new event queue.
    pub fn new() -> Self {
        Self {
            queue: ArrayQueue::new(EVENT_QUEUE_SIZE),
            waiters: WaitQueue::new(),
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
}

/// Global storage for window event queues.
pub static WINDOW_EVENTS: PreemptRwLock<BTreeMap<WindowId, Arc<WindowEventQueue>>> =
    PreemptRwLock::new(BTreeMap::new());

/// Last mouse button state for detecting changes.
static LAST_MOUSE_BUTTONS: PreemptSpinlock<u8> = PreemptSpinlock::new(0);

/// Initialize the input routing system.
pub fn init_input_routing() {
    queue_spawn_kthread_named("window-input", input_routing_thread as *const () as u64);
}

/// Get or create an event queue for a window.
pub fn get_or_create_event_queue(window_id: WindowId) -> Arc<WindowEventQueue> {
    {
        let queues = ranked_read!(RANK_WINDOW_EVENTS, "window::queue_lookup", WINDOW_EVENTS);
        if let Some(queue) = queues.get(&window_id) {
            return queue.clone();
        }
    }

    // Re-check under write lock: another CPU may have inserted while we dropped the read lock.
    let mut queues = ranked_write!(RANK_WINDOW_EVENTS, "window::queue_create", WINDOW_EVENTS);
    if let Some(queue) = queues.get(&window_id) {
        return queue.clone();
    }
    let queue = Arc::new(WindowEventQueue::new());
    queues.insert(window_id, queue.clone());
    queue
}

/// Remove the event queue for a window.
pub fn remove_event_queue(window_id: WindowId) {
    ranked_write!(RANK_WINDOW_EVENTS, "window::queue_remove", WINDOW_EVENTS).remove(&window_id);
}

/// Poll events for a window, returns up to `max` events.
pub fn poll_events(window_id: WindowId, max: usize) -> Vec<WindowEvent> {
    // Pre-allocate outside the lock to avoid heap allocation under spinlock.
    let mut events = Vec::with_capacity(max.min(EVENT_QUEUE_SIZE));
    let queues = ranked_read!(RANK_WINDOW_EVENTS, "window::poll_events", WINDOW_EVENTS);
    if let Some(queue) = queues.get(&window_id) {
        while events.len() < max {
            if let Some(event) = queue.pop() {
                events.push(event);
            } else {
                break;
            }
        }
    }
    events
}

/// Send an event to a specific window.
pub fn send_event(window_id: WindowId, event: WindowEvent) {
    // Cloned out of the map so the wake happens without the queue lock held:
    // waking takes the scheduler's lock, and the two must not be co-held.
    let queue = {
        let queues = ranked_read!(RANK_WINDOW_EVENTS, "window::send_event", WINDOW_EVENTS);
        queues.get(&window_id).cloned()
    };
    if let Some(queue) = queue {
        let _ = queue.push(event);
        queue.waiters.wake_all();
    }
}

/// Main input routing thread.
extern "C" fn input_routing_thread() -> ! {
    log!("Window input routing thread started");

    let thread = current_thread().unwrap();
    thread.set_priority(10); // High priority for input

    // Subscribe to input broadcasts
    let mouse_sub: Arc<Subscriber<MouseEvent>> = MOUSE_BROADCAST.subscribe();
    let key_sub: Arc<Subscriber<KeyEvent>> = KEY_EVENT_BROADCAST.subscribe();

    loop {
        thread_park_while(|| mouse_sub.is_empty() && key_sub.is_empty());

        while let Some(mouse_event) = mouse_sub.try_recv() {
            handle_mouse_event(mouse_event);
        }

        while let Some(key_event) = key_sub.try_recv() {
            handle_keyboard_event(key_event);
        }
    }
}

/// Handle a mouse event and route to appropriate window.
fn handle_mouse_event(event: MouseEvent) {
    let registry = read_tracked(ReadSite::HandleMouseEvent);

    // Find window whose CLIENT area contains the cursor (for event routing)
    let window_under_cursor = registry.window_at_client(event.x, event.y);

    // Find window under cursor including decorations (for focus changes).
    // Clicking a title bar should change focus even though it's not client area.
    let window_under_decorated = registry.window_at(event.x, event.y);

    // Get currently focused window
    let focused = registry.focused_window();

    // Track button state changes
    let mut last_buttons = ranked_lock!(
        RANK_MOUSE_BUTTONS,
        "window::mouse_buttons",
        LAST_MOUSE_BUTTONS
    );
    let buttons_changed = event.buttons != **last_buttons;
    let button_pressed = event.buttons & !**last_buttons;
    let button_released = !event.buttons & **last_buttons;
    **last_buttons = event.buttons;
    drop(last_buttons);

    // Handle focus change on mouse button press (uses decorated bounds so
    // clicking title bars, borders, and resize handles also changes focus).
    if button_pressed != 0
        && let Some(target_window) = window_under_decorated
        && focused != Some(target_window)
    {
        // Capture client-area coords before dropping the lock (for click event).
        let window_info = registry
            .get_window(target_window)
            .map(|w| (w.x, w.y, w.frame));
        drop(registry);

        {
            let mut registry = ranked_write!(
                RANK_WINDOW_REGISTRY,
                "window::focus_change",
                WINDOW_REGISTRY
            );
            // Re-verify window still exists under write lock
            if registry.get_window(target_window).is_some() {
                registry.set_focused(target_window);
            }
        }

        // Send focus events outside the lock
        if let Some(old_focused) = focused {
            send_event(old_focused, WindowEvent::focus_lost());
        }
        send_event(target_window, WindowEvent::focus_gained());

        // Send click event only if the click was in the client area
        if window_under_cursor == Some(target_window)
            && let Some((wx, wy, wframe)) = window_info
        {
            let local_x = event.x - wx - wframe.left;
            let local_y = event.y - wy - wframe.top;

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

    // Route mouse events to window under cursor (for move/scroll) or focused window (for buttons)
    if let Some(target) = window_under_cursor
        && let Some(window) = registry.get_window(target)
    {
        // Calculate coordinates relative to client area (excluding decorations)
        let local_x = event.x - window.x - window.frame.left;
        let local_y = event.y - window.y - window.frame.top;

        // Always send move events if there's movement
        if event.dx != 0 || event.dy != 0 {
            send_event(target, WindowEvent::mouse_move(local_x, local_y));
        }

        // Send scroll events
        if event.scroll != 0 {
            send_event(
                target,
                WindowEvent::mouse_scroll(local_x, local_y, event.scroll),
            );
        }

        // Send button events
        if buttons_changed {
            for bit in 0..3 {
                if button_pressed & (1 << bit) != 0 {
                    send_event(
                        target,
                        WindowEvent::mouse_button(local_x, local_y, bit, true),
                    );
                }
                if button_released & (1 << bit) != 0 {
                    send_event(
                        target,
                        WindowEvent::mouse_button(local_x, local_y, bit, false),
                    );
                }
            }
        }
    }
}

/// Handle a raw key event (press/release) and route to focused window.
///
/// A chord the session shell has claimed is withheld from the focused window;
/// the shell reads the same key off the `/dev/kbd` broadcast, which this does
/// not touch.
fn handle_keyboard_event(event: KeyEvent) {
    let code = event.code as u32;
    let down = !matches!(event.state, KeyState::Up);
    let claimed = super::grab::intercept(code, down);
    if matches!(event.state, KeyState::SingleShot) {
        // Press and release arrive as one event, so a withheld press here has
        // no later release to clear the record.
        super::grab::intercept(code, false);
    }
    if claimed {
        return;
    }

    let registry = read_tracked(ReadSite::HandleKeyboardEvent);

    if let Some(focused_id) = registry.focused_window() {
        match event.state {
            KeyState::Down => send_event(focused_id, WindowEvent::key_press(code)),
            KeyState::Up => send_event(focused_id, WindowEvent::key_release(code)),
            KeyState::SingleShot => {
                send_event(focused_id, WindowEvent::key_press(code));
                send_event(focused_id, WindowEvent::key_release(code));
            }
        }
    }
}
