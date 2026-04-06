//! EDOS Window Manager - User-space compositor.

use std::time::{Duration, Instant};

use edos_render::graphics::Screen;
use edos_render::window::{
    WindowEvent, WindowEventType, WindowListEntry, property, window_list, window_send_event,
    window_set,
};

mod compositor;
mod cursor;
mod decorations;
mod dirty;
mod input;

use compositor::ShmCache;
use cursor::{Cursor, CursorShape};
use decorations::HitRegion;
use dirty::{DirtyRect, DirtyRegion};
use input::{InputAction, InputState};

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

/// Handle a left mouse button press: hit-test the topmost window and start
/// drag, resize, close, or focus accordingly.
fn handle_mouse_press(
    windows: &[WindowListEntry],
    mx: i32,
    my: i32,
    drag_state: &mut Option<DragState>,
    resize_state: &mut Option<ResizeState>,
    focused_window_id: &mut Option<u64>,
) {
    let window = match find_window_at(windows, mx, my) {
        Some(w) => w,
        None => return,
    };
    let window_id = window.id;
    let region = decorations::hit_test(window, mx, my);

    match region {
        HitRegion::CloseButton => {
            let _ = window_send_event(window_id, &WindowEvent::close_requested());
        }
        HitRegion::TitleBar => {
            *drag_state = Some(DragState {
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
            *resize_state = Some(ResizeState {
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

    *focused_window_id = Some(window_id);
}

/// Update window position while dragging, or stop if mouse released.
fn handle_drag(drag_state: &mut Option<DragState>, mx: i32, my: i32, left_held: bool) {
    let drag = match *drag_state {
        Some(ref d) => d,
        None => return,
    };

    if left_held {
        let new_x = mx - drag.offset_x;
        let new_y = my - drag.offset_y;
        let _ = window_set(drag.window_id, property::X, new_x as i64 as u64);
        let _ = window_set(drag.window_id, property::Y, new_y as i64 as u64);
    } else {
        *drag_state = None;
    }
}

/// Update window dimensions while resizing, or finalize and send resize event.
fn handle_resize(resize_state: &mut Option<ResizeState>, mx: i32, my: i32, left_held: bool) {
    let resize = match *resize_state {
        Some(ref r) => r,
        None => return,
    };

    let dx = mx - resize.start_x;
    let dy = my - resize.start_y;

    let (mut new_x, mut new_y, mut new_w, mut new_h) = (
        resize.orig_win_x,
        resize.orig_win_y,
        resize.orig_win_w as i32,
        resize.orig_win_h as i32,
    );

    match resize.region {
        HitRegion::ResizeRight => new_w += dx,
        HitRegion::ResizeBottom => new_h += dy,
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

    let win_id = resize.window_id;
    let region = resize.region;

    if left_held {
        let _ = window_set(win_id, property::X, new_x as i64 as u64);
        let _ = window_set(win_id, property::Y, new_y as i64 as u64);
        let _ = window_set(win_id, property::WIDTH, new_w as i64 as u64);
        let _ = window_set(win_id, property::HEIGHT, new_h as i64 as u64);
    } else {
        let resize_event = WindowEvent {
            event_type: WindowEventType::Resize as u32,
            x: new_w,
            y: new_h,
            code: 0,
            data: 0,
        };
        let _ = window_send_event(win_id, &resize_event);
        *resize_state = None;
    }
}

/// Invalidate drag/resize/focus state if their target windows no longer exist.
fn validate_window_state(
    windows: &[WindowListEntry],
    drag_state: &mut Option<DragState>,
    resize_state: &mut Option<ResizeState>,
    focused_window_id: &mut Option<u64>,
) {
    if let Some(ref drag) = *drag_state {
        if !windows.iter().any(|w| w.id == drag.window_id) {
            *drag_state = None;
        }
    }
    if let Some(ref resize) = *resize_state {
        if !windows.iter().any(|w| w.id == resize.window_id) {
            *resize_state = None;
        }
    }
    if let Some(fid) = *focused_window_id {
        if !windows.iter().any(|w| w.id == fid) {
            *focused_window_id = windows
                .iter()
                .filter(|w| w.visible != 0)
                .max_by_key(|w| w.z_order)
                .map(|w| w.id);
        }
    }
}

/// Determine the cursor shape based on current interaction state and hover position.
fn determine_cursor_shape(
    windows: &[WindowListEntry],
    drag_state: &Option<DragState>,
    resize_state: &Option<ResizeState>,
    cx: i32,
    cy: i32,
) -> CursorShape {
    if drag_state.is_some() {
        return CursorShape::Arrow;
    }

    if let Some(ref rs) = *resize_state {
        return match rs.region {
            HitRegion::ResizeLeft | HitRegion::ResizeRight => CursorShape::ResizeH,
            HitRegion::ResizeTop | HitRegion::ResizeBottom => CursorShape::ResizeV,
            HitRegion::ResizeTopLeft | HitRegion::ResizeBottomRight => CursorShape::ResizeFDiag,
            HitRegion::ResizeTopRight | HitRegion::ResizeBottomLeft => CursorShape::ResizeBDiag,
            _ => CursorShape::Arrow,
        };
    }

    for window in windows.iter().rev() {
        if window.visible == 0 {
            continue;
        }
        let region = decorations::hit_test(window, cx, cy);
        if region != HitRegion::None {
            return match region {
                HitRegion::ResizeLeft | HitRegion::ResizeRight => CursorShape::ResizeH,
                HitRegion::ResizeTop | HitRegion::ResizeBottom => CursorShape::ResizeV,
                HitRegion::ResizeTopLeft | HitRegion::ResizeBottomRight => CursorShape::ResizeFDiag,
                HitRegion::ResizeTopRight | HitRegion::ResizeBottomLeft => CursorShape::ResizeBDiag,
                _ => CursorShape::Arrow,
            };
        }
    }

    CursorShape::Arrow
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
    let mut hovered_close_window: Option<u64> = None;

    // Shared memory mapping cache
    let mut shm_cache = ShmCache::new();

    // Input device state (mouse + keyboard)
    let mut input = InputState::new();

    // Dirty region tracking
    let mut dirty = DirtyRegion::new();
    // Force full screen on the first frame.
    dirty.mark_full_screen();

    // Previous frame window snapshots for change detection.
    let prev_windows: [Option<PrevWindowState>; MAX_WINDOWS] = [None; MAX_WINDOWS];
    let prev_window_count: usize = 0;

    // Previous cursor position for dirty rect invalidation.
    let prev_cursor_x: i32 = 0;
    let prev_cursor_y: i32 = 0;

    // Main compositor loop
    loop {
        let frame_start = Instant::now();

        // Read mouse state
        let (mx, my, buttons) = input.read_mouse();
        cursor.set_position(mx, my);

        // Get current window list from kernel
        let window_count = match window_list(&mut entries) {
            Ok(count) => count.min(MAX_WINDOWS),
            Err(_) => 0,
        };

        let windows = &mut entries[..window_count];

        // Process keyboard shortcuts (after window list so Alt+Tab has current data)
        match input.read_keyboard(focused_window_id, windows) {
            InputAction::AltF4 { focused_id } => {
                let _ = window_send_event(focused_id, &WindowEvent::close_requested());
            }
            InputAction::AltTab { next_id } => {
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
            InputAction::None => {}
        }

        // Detect mouse button transitions
        let left_pressed = input.detect_left_press(buttons);
        let left_held = InputState::left_held(buttons);

        // Handle mouse interactions
        if left_pressed {
            handle_mouse_press(
                windows,
                mx,
                my,
                &mut drag_state,
                &mut resize_state,
                &mut focused_window_id,
            );
        }
        handle_drag(&mut drag_state, mx, my, left_held);
        handle_resize(&mut resize_state, mx, my, left_held);

        // Re-fetch window list after potential modifications
        let window_count = match window_list(&mut entries) {
            Ok(count) => count.min(MAX_WINDOWS),
            Err(_) => 0,
        };

        let windows = &entries[..window_count];

        // Validate interaction state against current window list
        validate_window_state(
            windows,
            &mut drag_state,
            &mut resize_state,
            &mut focused_window_id,
        );

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

        // Update cursor shape
        cursor.set_shape(determine_cursor_shape(
            windows,
            &drag_state,
            &resize_state,
            cursor.x,
            cursor.y,
        ));

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

        // Sleep remainder of frame budget to maintain frame rate.
        // Use a minimum sleep of 1ms to avoid sub-microsecond sleeps that
        // can interact badly with the scheduler.
        let frame_target = Duration::from_millis(FRAME_TIME_MS);
        let elapsed = frame_start.elapsed();
        if elapsed < frame_target {
            let remaining = frame_target - elapsed;
            if remaining > Duration::from_millis(1) {
                std::thread::sleep(remaining);
            } else {
                std::thread::yield_now();
            }
        }
    }
}
