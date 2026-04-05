//! EDOS Window Manager - User-space compositor.

use std::time::Duration;

use edos_render::graphics::Screen;
use edos_render::window::{
    property, read_mouse_state, window_list, window_send_event, window_set, WindowEvent,
    WindowListEntry,
};

mod compositor;
mod cursor;
mod decorations;

use compositor::ShmCache;
use cursor::Cursor;
use decorations::HitRegion;

/// Maximum number of windows to track.
const MAX_WINDOWS: usize = 64;

/// Target frame time (approximately 60 FPS).
const FRAME_TIME_MS: u64 = 16;

/// Minimum window width in pixels.
const MIN_WINDOW_WIDTH: u32 = 100;

/// Minimum window height in pixels.
const MIN_WINDOW_HEIGHT: u32 = 50;

/// Track window dragging state.
struct DragState {
    window_id: u64,
    offset_x: i32, // cursor offset from window origin
    offset_y: i32,
}

/// Track window resize state.
struct ResizeState {
    window_id: u64,
    region: HitRegion,
    start_x: i32,
    start_y: i32,
    orig_win_x: i32,
    orig_win_y: i32,
    orig_win_w: u32,
    orig_win_h: u32,
}

/// Find the topmost window under the cursor.
fn find_window_at(windows: &[WindowListEntry], x: i32, y: i32) -> Option<&WindowListEntry> {
    windows
        .iter()
        .filter(|w| w.visible != 0)
        .filter(|w| {
            let total_w = decorations::effective_width(w) as i32;
            let total_h = decorations::effective_height(w) as i32;
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
    let mut resize_state: Option<ResizeState> = None;
    let mut last_mouse_buttons: u8 = 0;

    // Shared memory mapping cache
    let mut shm_cache = ShmCache::new();

    // Open mouse device once (not every frame)
    let mut mouse_file = std::fs::File::open("/dev/mouse").expect("failed to open /dev/mouse");

    // Main compositor loop
    loop {
        // Get mouse state (position + buttons), default to (0, 0, 0) if unavailable
        let (mx, my, buttons) = read_mouse_state(&mut mouse_file).unwrap_or((0, 0, 0));

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
                let region = decorations::hit_test(window, mx, my);

                match region {
                    HitRegion::CloseButton => {
                        // Send close request to the window
                        println!("[WM] Sending CloseRequested to window {}", window_id);
                        let event = WindowEvent::close_requested();
                        let _ = window_send_event(window_id, &event);
                    }
                    HitRegion::TitleBar => {
                        // Start dragging
                        drag_state = Some(DragState {
                            window_id,
                            offset_x: mx - window.x,
                            offset_y: my - window.y,
                        });
                    }
                    HitRegion::ResizeTop
                    | HitRegion::ResizeBottom
                    | HitRegion::ResizeLeft
                    | HitRegion::ResizeRight
                    | HitRegion::ResizeTopLeft
                    | HitRegion::ResizeTopRight
                    | HitRegion::ResizeBottomLeft
                    | HitRegion::ResizeBottomRight => {
                        // Start resizing
                        resize_state = Some(ResizeState {
                            window_id,
                            region,
                            start_x: mx,
                            start_y: my,
                            orig_win_x: window.x,
                            orig_win_y: window.y,
                            orig_win_w: window.width,
                            orig_win_h: window.height,
                        });
                    }
                    _ => {}
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

        // Handle resizing
        if let Some(ref resize) = resize_state {
            if left_held {
                let dx = mx - resize.start_x;
                let dy = my - resize.start_y;

                // Calculate new position and dimensions based on which region is being dragged
                let (mut new_x, mut new_y, mut new_w, mut new_h) = (
                    resize.orig_win_x,
                    resize.orig_win_y,
                    resize.orig_win_w as i32,
                    resize.orig_win_h as i32,
                );

                match resize.region {
                    HitRegion::ResizeRight => {
                        new_w += dx;
                    }
                    HitRegion::ResizeBottom => {
                        new_h += dy;
                    }
                    HitRegion::ResizeLeft => {
                        new_x += dx;
                        new_w -= dx;
                    }
                    HitRegion::ResizeTop => {
                        new_y += dy;
                        new_h -= dy;
                    }
                    HitRegion::ResizeBottomRight => {
                        new_w += dx;
                        new_h += dy;
                    }
                    HitRegion::ResizeTopLeft => {
                        new_x += dx;
                        new_y += dy;
                        new_w -= dx;
                        new_h -= dy;
                    }
                    HitRegion::ResizeTopRight => {
                        new_y += dy;
                        new_w += dx;
                        new_h -= dy;
                    }
                    HitRegion::ResizeBottomLeft => {
                        new_x += dx;
                        new_w -= dx;
                        new_h += dy;
                    }
                    _ => {}
                }

                // Enforce minimum window size
                if new_w < MIN_WINDOW_WIDTH as i32 {
                    // Adjust position if resizing from left side
                    if matches!(
                        resize.region,
                        HitRegion::ResizeLeft
                            | HitRegion::ResizeTopLeft
                            | HitRegion::ResizeBottomLeft
                    ) {
                        new_x = resize.orig_win_x + resize.orig_win_w as i32 - MIN_WINDOW_WIDTH as i32;
                    }
                    new_w = MIN_WINDOW_WIDTH as i32;
                }
                if new_h < MIN_WINDOW_HEIGHT as i32 {
                    // Adjust position if resizing from top side
                    if matches!(
                        resize.region,
                        HitRegion::ResizeTop | HitRegion::ResizeTopLeft | HitRegion::ResizeTopRight
                    ) {
                        new_y = resize.orig_win_y + resize.orig_win_h as i32 - MIN_WINDOW_HEIGHT as i32;
                    }
                    new_h = MIN_WINDOW_HEIGHT as i32;
                }

                // Apply the new dimensions
                let _ = window_set(resize.window_id, property::X, new_x as i64 as u64);
                let _ = window_set(resize.window_id, property::Y, new_y as i64 as u64);
                let _ = window_set(resize.window_id, property::WIDTH, new_w as u64);
                let _ = window_set(resize.window_id, property::HEIGHT, new_h as u64);
            } else {
                // Mouse released - stop resizing
                // Send resize event to the window with final dimensions
                // First get the current dimensions from kernel
                if let Some(win) = windows.iter().find(|w| w.id == resize.window_id) {
                    let resize_event = WindowEvent {
                        event_type: 10, // WindowEventType::Resize
                        x: win.width as i32,
                        y: win.height as i32,
                        code: 0,
                        data: 0,
                    };
                    let _ = window_send_event(resize.window_id, &resize_event);
                }
                resize_state = None;
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
        compositor::composite(&mut screen, windows, &cursor, focused_window_id, &mut shm_cache);
        let _ = screen.render();

        // Sleep to maintain frame rate
        std::thread::sleep(Duration::from_millis(FRAME_TIME_MS));
    }
}
