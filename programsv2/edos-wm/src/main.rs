//! EDOS Window Manager - User-space compositor.

use std::time::Duration;

use edos_render::graphics::Screen;
use edos_render::window::{
    get_mouse_state, property, window_destroy, window_list, window_set, WindowListEntry,
};

mod compositor;
mod cursor;
mod decorations;

use cursor::Cursor;

/// Maximum number of windows to track.
const MAX_WINDOWS: usize = 64;

/// Target frame time (approximately 60 FPS).
const FRAME_TIME_MS: u64 = 16;

/// Track window dragging state.
struct DragState {
    window_id: u64,
    offset_x: i32, // cursor offset from window origin
    offset_y: i32,
}

/// Find the topmost window under the cursor.
fn find_window_at(windows: &[WindowListEntry], x: i32, y: i32) -> Option<&WindowListEntry> {
    windows
        .iter()
        .filter(|w| w.visible != 0)
        .filter(|w| {
            let total_w = decorations::decorated_width(w.width) as i32;
            let total_h = decorations::decorated_height(w.height) as i32;
            x >= w.x && x < w.x + total_w && y >= w.y && y < w.y + total_h
        })
        .max_by_key(|w| w.z_order)
}

fn main() {
    // Initialize screen
    let mut screen = match Screen::get() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to initialize screen: {:?}", e);
            return;
        }
    };

    // Initialize cursor
    let mut cursor = Cursor::new();

    // Window list buffer
    let mut entries = [WindowListEntry::default(); MAX_WINDOWS];

    // Track focused window
    let mut focused_window_id: Option<u64> = None;

    // Interaction state
    let mut drag_state: Option<DragState> = None;
    let mut last_mouse_buttons: u8 = 0;

    // Main compositor loop
    loop {
        // Get mouse state (position + buttons)
        let (mx, my, buttons) = match get_mouse_state() {
            Some(state) => state,
            None => {
                std::thread::sleep(Duration::from_millis(FRAME_TIME_MS));
                continue;
            }
        };

        cursor.set_position(mx, my);

        // Get current window list from kernel
        let window_count = match window_list(&mut entries) {
            Ok(count) => count.min(MAX_WINDOWS),
            Err(_) => 0,
        };

        let windows = &mut entries[..window_count];

        // Detect left button press (transition from not pressed to pressed)
        let left_pressed = (buttons & 0x01) != 0 && (last_mouse_buttons & 0x01) == 0;
        let left_held = (buttons & 0x01) != 0;

        // Handle mouse button press
        if left_pressed {
            if let Some(window) = find_window_at(windows, mx, my) {
                let window_id = window.id;

                if decorations::is_in_close_button(window, mx, my) {
                    // Close the window
                    let _ = window_destroy(window_id);
                } else if decorations::is_in_title_bar(window, mx, my) {
                    // Start dragging
                    drag_state = Some(DragState {
                        window_id,
                        offset_x: mx - window.x,
                        offset_y: my - window.y,
                    });
                }

                // Update focus to clicked window
                focused_window_id = Some(window_id);
            }
        }

        // Handle dragging
        if let Some(ref drag) = drag_state {
            if left_held {
                // Mouse still held - update window position
                let new_x = mx - drag.offset_x;
                let new_y = my - drag.offset_y;
                let _ = window_set(drag.window_id, property::X, new_x as i64 as u64);
                let _ = window_set(drag.window_id, property::Y, new_y as i64 as u64);
            } else {
                // Mouse released - stop dragging
                drag_state = None;
            }
        }

        last_mouse_buttons = buttons;

        // Re-fetch window list after potential modifications
        let window_count = match window_list(&mut entries) {
            Ok(count) => count.min(MAX_WINDOWS),
            Err(_) => 0,
        };

        let windows = &entries[..window_count];

        // Validate focused window still exists
        if let Some(fid) = focused_window_id {
            if !windows.iter().any(|w| w.id == fid) {
                // Focused window was destroyed, pick new top window
                focused_window_id = windows
                    .iter()
                    .filter(|w| w.visible != 0)
                    .max_by_key(|w| w.z_order)
                    .map(|w| w.id);
            }
        }

        // Composite all windows and present
        compositor::composite(&mut screen, windows, &cursor, focused_window_id);
        let _ = screen.render();

        // Sleep to maintain frame rate
        std::thread::sleep(Duration::from_millis(FRAME_TIME_MS));
    }
}
