//! EDOS Window Manager - User-space compositor.

use std::io::Read;
use std::time::{Duration, Instant};

use edos_render::graphics::Screen;
use edos_render::window::{
    WindowEvent, WindowEventType, WindowListEntry, flags::FLAG_DOCK, property, read_mouse_state,
    window_list, window_send_event, window_set,
};

mod compositor;
mod cursor;
mod decorations;
mod dirty;

use compositor::ShmCache;
use cursor::{Cursor, CursorShape};
use decorations::HitRegion;
use dirty::{DirtyRect, DirtyRegion};

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

/// Cursor size for dirty rect marking.
const CURSOR_SIZE: u32 = 16;

/// Snapshot of a window's position/size/visibility from the previous frame.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PrevWindowState {
    id: u64,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    visible: u32,
    flags: u64,
}

impl PrevWindowState {
    fn from_entry(w: &WindowListEntry) -> Self {
        Self {
            id: w.id,
            x: w.x,
            y: w.y,
            width: w.width,
            height: w.height,
            visible: w.visible,
            flags: w.flags,
        }
    }

    fn dirty_rect(&self) -> DirtyRect {
        let eff_w = decorations::effective_width_raw(self.flags, self.width);
        let eff_h = decorations::effective_height_raw(self.flags, self.height);
        DirtyRect::new(self.x, self.y, eff_w as u32, eff_h as u32)
    }
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
    let mut hovered_close_window: Option<u64> = None;

    // Shared memory mapping cache
    let mut shm_cache = ShmCache::new();

    // Keyboard modifier state
    let mut alt_held = false;

    // Open mouse device once (not every frame)
    let mut mouse_file = std::fs::File::open("/dev/mouse").expect("failed to open /dev/mouse");

    // Open keyboard device (non-fatal if unavailable)
    let mut kbd_file = std::fs::File::open("/dev/kbd").ok();

    // Dirty region tracking
    let mut dirty = DirtyRegion::new();
    // Force full screen on the first frame.
    dirty.mark_full_screen();

    // Previous frame window snapshots for change detection.
    let mut prev_windows: [Option<PrevWindowState>; MAX_WINDOWS] = [None; MAX_WINDOWS];
    let mut prev_window_count: usize = 0;

    // Previous cursor position for dirty rect invalidation.
    let mut prev_cursor_x: i32 = 0;
    let mut prev_cursor_y: i32 = 0;

    // Main compositor loop
    loop {
        let frame_start = Instant::now();

        // Get mouse state (position + buttons), default to (0, 0, 0) if unavailable
        let (mx, my, buttons) = read_mouse_state(&mut mouse_file).unwrap_or((0, 0, 0));

        cursor.set_position(mx, my);

        // Read keyboard events (non-blocking: drains whatever is buffered)
        if let Some(ref mut kbd) = kbd_file {
            let mut kbd_buf = [0u8; 64]; // room for 16 key events
            if let Ok(n) = kbd.read(&mut kbd_buf) {
                const RAW_LALT: u32 = 0x8000_005F;
                const RAW_F4: u32 = 0x8000_0004;

                let mut i = 0;
                while i + 4 <= n {
                    let key = u32::from_le_bytes([
                        kbd_buf[i],
                        kbd_buf[i + 1],
                        kbd_buf[i + 2],
                        kbd_buf[i + 3],
                    ]);
                    i += 4;

                    if key == RAW_LALT {
                        alt_held = true;
                        continue;
                    }

                    if alt_held {
                        if key == RAW_F4 {
                            // Alt+F4: send close request to focused window
                            if let Some(fid) = focused_window_id {
                                let close_event = WindowEvent {
                                    event_type: WindowEventType::CloseRequested as u32,
                                    x: 0,
                                    y: 0,
                                    code: 0,
                                    data: 0,
                                };
                                let _ = window_send_event(fid, &close_event);
                            }
                            alt_held = false;
                            continue;
                        }

                        if key == 0x09 {
                            // Alt+Tab: cycle focus to next visible non-dock window
                            let mut tab_entries = [WindowListEntry::default(); MAX_WINDOWS];
                            let tab_count = match window_list(&mut tab_entries) {
                                Ok(c) => c.min(MAX_WINDOWS),
                                Err(_) => 0,
                            };
                            let visible: Vec<u64> = tab_entries[..tab_count]
                                .iter()
                                .filter(|w| w.visible != 0 && (w.flags & FLAG_DOCK) == 0)
                                .map(|w| w.id)
                                .collect();

                            if !visible.is_empty() {
                                let current_idx = focused_window_id
                                    .and_then(|fid| visible.iter().position(|&id| id == fid))
                                    .unwrap_or(0);
                                let next_idx = (current_idx + 1) % visible.len();
                                let next_id = visible[next_idx];

                                let focus_event = WindowEvent {
                                    event_type: WindowEventType::FocusGained as u32,
                                    x: 0,
                                    y: 0,
                                    code: 0,
                                    data: 0,
                                };
                                let _ = window_send_event(next_id, &focus_event);
                                focused_window_id = Some(next_id);
                            }
                            alt_held = false;
                            continue;
                        }

                        // Unrecognized key while Alt held: clear Alt state
                        alt_held = false;
                    }
                }
            }
        }

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
                if matches!(
                    resize.region,
                    HitRegion::ResizeLeft | HitRegion::ResizeTopLeft | HitRegion::ResizeBottomLeft
                ) {
                    new_x = resize.orig_win_x + resize.orig_win_w as i32 - MIN_WINDOW_WIDTH as i32;
                }
                new_w = MIN_WINDOW_WIDTH as i32;
            }
            if new_h < MIN_WINDOW_HEIGHT as i32 {
                if matches!(
                    resize.region,
                    HitRegion::ResizeTop | HitRegion::ResizeTopLeft | HitRegion::ResizeTopRight
                ) {
                    new_y = resize.orig_win_y + resize.orig_win_h as i32 - MIN_WINDOW_HEIGHT as i32;
                }
                new_h = MIN_WINDOW_HEIGHT as i32;
            }

            if left_held {
                // Apply the new dimensions
                let _ = window_set(resize.window_id, property::X, new_x as i64 as u64);
                let _ = window_set(resize.window_id, property::Y, new_y as i64 as u64);
                let _ = window_set(resize.window_id, property::WIDTH, new_w as i64 as u64);
                let _ = window_set(resize.window_id, property::HEIGHT, new_h as i64 as u64);
            } else {
                // Mouse released - send resize event with final clamped dimensions
                let resize_event = WindowEvent {
                    event_type: WindowEventType::Resize as u32,
                    x: new_w,
                    y: new_h,
                    code: 0,
                    data: 0,
                };
                let _ = window_send_event(resize.window_id, &resize_event);
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

        // Invalidate drag/resize state if target window was destroyed
        if let Some(ref drag) = drag_state {
            if !windows.iter().any(|w| w.id == drag.window_id) {
                drag_state = None;
            }
        }
        if let Some(ref resize) = resize_state {
            if !windows.iter().any(|w| w.id == resize.window_id) {
                resize_state = None;
            }
        }

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

        // Detect dirty regions from window changes relative to previous frame.
        let screen_w = screen.width() as u32;
        let screen_h = screen.height() as u32;

        if !dirty.full_screen {
            // Mark all visible window rects dirty unconditionally.
            // We have no damage signal from clients, so any window could have
            // repainted its shared memory buffer without changing geometry.
            for w in windows.iter() {
                if w.visible != 0 {
                    let s = PrevWindowState::from_entry(w);
                    if let Some(r) = s.dirty_rect().clipped(screen_w, screen_h) {
                        dirty.mark_dirty(r);
                    }
                }
            }

            // Mark old rects of moved or disappeared windows (exposes background).
            for slot in prev_windows[..prev_window_count].iter().flatten() {
                let still_here = windows
                    .iter()
                    .any(|w| w.id == slot.id && w.x == slot.x && w.y == slot.y);
                if !still_here {
                    if let Some(r) = slot.dirty_rect().clipped(screen_w, screen_h) {
                        dirty.mark_dirty(r);
                    }
                }
            }

            // Mark cursor regions dirty when cursor moved.
            if prev_cursor_x != cursor.x || prev_cursor_y != cursor.y {
                if let Some(r) =
                    DirtyRect::new(prev_cursor_x, prev_cursor_y, CURSOR_SIZE, CURSOR_SIZE)
                        .clipped(screen_w, screen_h)
                {
                    dirty.mark_dirty(r);
                }
                if let Some(r) = DirtyRect::new(cursor.x, cursor.y, CURSOR_SIZE, CURSOR_SIZE)
                    .clipped(screen_w, screen_h)
                {
                    dirty.mark_dirty(r);
                }
            }
        }

        // Compute which window's close button the cursor is hovering over.
        // Windows are sorted back-to-front by z_order, so iterate in reverse
        // (front-to-back) and take the first close-button hit.
        hovered_close_window = windows
            .iter()
            .filter(|w| w.visible != 0)
            .rev()
            .find(|w| {
                decorations::hit_test(w, cursor.x, cursor.y) == decorations::HitRegion::CloseButton
            })
            .map(|w| w.id);

        // Determine cursor shape based on what's under the cursor.
        let cursor_shape = if drag_state.is_some() {
            // Keep arrow during drag
            CursorShape::Arrow
        } else if let Some(ref rs) = resize_state {
            // During active resize, keep the resize cursor for the active region
            match rs.region {
                HitRegion::ResizeLeft | HitRegion::ResizeRight => CursorShape::ResizeH,
                HitRegion::ResizeTop | HitRegion::ResizeBottom => CursorShape::ResizeV,
                HitRegion::ResizeTopLeft | HitRegion::ResizeBottomRight => CursorShape::ResizeFDiag,
                HitRegion::ResizeTopRight | HitRegion::ResizeBottomLeft => CursorShape::ResizeBDiag,
                _ => CursorShape::Arrow,
            }
        } else {
            // Check which hit region the cursor is hovering over
            let mut shape = CursorShape::Arrow;
            for window in windows.iter().rev() {
                if window.visible == 0 {
                    continue;
                }
                let region = decorations::hit_test(window, cursor.x, cursor.y);
                if region != HitRegion::None {
                    shape = match region {
                        HitRegion::ResizeLeft | HitRegion::ResizeRight => CursorShape::ResizeH,
                        HitRegion::ResizeTop | HitRegion::ResizeBottom => CursorShape::ResizeV,
                        HitRegion::ResizeTopLeft | HitRegion::ResizeBottomRight => {
                            CursorShape::ResizeFDiag
                        }
                        HitRegion::ResizeTopRight | HitRegion::ResizeBottomLeft => {
                            CursorShape::ResizeBDiag
                        }
                        _ => CursorShape::Arrow,
                    };
                    break;
                }
            }
            shape
        };
        cursor.set_shape(cursor_shape);

        // Composite all windows into back buffer (always full composite).
        compositor::composite(
            &mut screen,
            windows,
            &cursor,
            focused_window_id,
            &mut shm_cache,
            hovered_close_window,
        );

        // Send full back buffer to kernel framebuffer and flip.
        // Dirty-rect partial updates are disabled while double buffering is
        // active because the front-to-back page sync needed after each flip
        // costs as much as a full draw. The proper fix is mmap'ing VRAM into
        // userspace so the compositor writes directly to the back page.
        let _ = screen.render();

        // Sleep remainder of frame budget to maintain frame rate
        let frame_target = Duration::from_millis(FRAME_TIME_MS);
        let elapsed = frame_start.elapsed();
        if elapsed < frame_target {
            std::thread::sleep(frame_target - elapsed);
        }
    }
}
