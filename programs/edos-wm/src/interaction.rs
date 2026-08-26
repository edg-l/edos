//! Window interaction: what a press, a drag and a resize do to the window list,
//! and which cursor shape the pointer's position implies.

use std::collections::HashMap;

use edos_render::window::{
    WindowEvent, WindowEventType, WindowListEntry, property, window_minimize, window_send_event,
    window_set,
};

use crate::cursor::CursorShape;
use crate::decorations::{self, HitRegion};

/// Height of the panel, which a maximized window must not cover.
///
/// The panel owns this number; duplicated here because there is no protocol
/// for a panel to reserve a strut yet, and a maximized window that hides it
/// leaves no way back to any other window.
const PANEL_HEIGHT: u32 = 40;
/// Minimum window width in pixels.
const MIN_WINDOW_WIDTH: u32 = 100;

/// Minimum window height in pixels.
const MIN_WINDOW_HEIGHT: u32 = 50;

/// Track window dragging state.
pub struct DragState {
    window_id: u64,
    offset_x: i32, // cursor offset from window origin
    offset_y: i32,
}

/// Track window resize state.
pub struct ResizeState {
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
pub fn find_window_at(windows: &[WindowListEntry], x: i32, y: i32) -> Option<&WindowListEntry> {
    windows
        .iter()
        .filter(|w| w.on_screen())
        .filter(|w| {
            let total_w = decorations::effective_width(w) as i32;
            let total_h = decorations::effective_height(w) as i32;
            x >= w.x && x < w.x + total_w && y >= w.y && y < w.y + total_h
        })
        .max_by_key(|w| w.z_order)
}

/// Handle a left mouse button press: hit-test the topmost window and start
/// drag, resize, or close accordingly.
///
/// Focus itself is the kernel registry's: it already moves focus to the window
/// under a press. Only the desktop-background case has to be reported, since
/// the kernel leaves focus alone when no window is hit.
#[allow(clippy::too_many_arguments)]
pub fn handle_mouse_press(
    windows: &[WindowListEntry],
    mx: i32,
    my: i32,
    drag_state: &mut Option<DragState>,
    resize_state: &mut Option<ResizeState>,
    focused_window_id: Option<u64>,
    screen_w: u32,
    screen_h: u32,
    maximized: &mut HashMap<u64, (i32, i32, u32, u32)>,
) {
    let window = match find_window_at(windows, mx, my) {
        Some(w) => w,
        None => {
            if let Some(old_id) = focused_window_id {
                let lost_event = WindowEvent {
                    event_type: WindowEventType::FocusLost as u32,
                    x: 0,
                    y: 0,
                    code: 0,
                    data: 0,
                };
                let _ = window_send_event(old_id, &lost_event);
            }
            return;
        }
    };
    let window_id = window.id;
    let region = decorations::hit_test(window, mx, my);

    match region {
        HitRegion::CloseButton => {
            let _ = window_send_event(window_id, &WindowEvent::close_requested());
        }
        HitRegion::MinimizeButton => {
            // The panel keeps the button, so there is a way back.
            let _ = window_minimize(window_id, true);
        }
        HitRegion::MaximizeButton => {
            toggle_maximized(window, screen_w, screen_h, maximized);
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
}

/// Update window position while dragging, or stop if mouse released.
pub fn handle_drag(drag_state: &mut Option<DragState>, mx: i32, my: i32, left_held: bool) {
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
pub fn handle_resize(resize_state: &mut Option<ResizeState>, mx: i32, my: i32, left_held: bool) {
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
/// Fill the working area, or put the window back where it was.
///
/// The previous geometry is remembered here rather than in the kernel: what
/// counts as "restored" is a window manager's policy, and the kernel has no
/// business holding a second position for a window.
fn toggle_maximized(
    window: &WindowListEntry,
    screen_w: u32,
    screen_h: u32,
    maximized: &mut HashMap<u64, (i32, i32, u32, u32)>,
) {
    let id = window.id;
    if let Some((x, y, w, h)) = maximized.remove(&id) {
        let _ = window_set(id, property::X, x as i64 as u64);
        let _ = window_set(id, property::Y, y as i64 as u64);
        let _ = window_set(id, property::WIDTH, w as u64);
        let _ = window_set(id, property::HEIGHT, h as u64);
        let event = WindowEvent {
            event_type: WindowEventType::Resize as u32,
            x: w as i32,
            y: h as i32,
            code: 0,
            data: 0,
        };
        let _ = window_send_event(id, &event);
        return;
    }

    maximized.insert(id, (window.x, window.y, window.width, window.height));

    // The working area, not the screen: a maximized window that covers the
    // panel hides the only way to get back to any other window.
    let frame_w = decorations::BORDER_WIDTH as u32 * 2;
    let frame_h = decorations::TITLE_HEIGHT as u32 + decorations::BORDER_WIDTH as u32;
    let avail_h = screen_h.saturating_sub(PANEL_HEIGHT);
    let new_w = screen_w.saturating_sub(frame_w).max(1);
    let new_h = avail_h.saturating_sub(frame_h).max(1);

    let _ = window_set(id, property::X, 0);
    let _ = window_set(id, property::Y, 0);
    let _ = window_set(id, property::WIDTH, new_w as u64);
    let _ = window_set(id, property::HEIGHT, new_h as u64);
    let event = WindowEvent {
        event_type: WindowEventType::Resize as u32,
        x: new_w as i32,
        y: new_h as i32,
        code: 0,
        data: 0,
    };
    let _ = window_send_event(id, &event);
}

/// Invalidate drag/resize state if their target windows no longer exist.
///
/// Focus needs no equivalent: the registry re-focuses the topmost survivor when
/// a window is destroyed.
pub fn validate_window_state(
    windows: &[WindowListEntry],
    drag_state: &mut Option<DragState>,
    resize_state: &mut Option<ResizeState>,
) {
    if let Some(ref drag) = *drag_state
        && !windows.iter().any(|w| w.id == drag.window_id)
    {
        *drag_state = None;
    }
    if let Some(ref resize) = *resize_state
        && !windows.iter().any(|w| w.id == resize.window_id)
    {
        *resize_state = None;
    }
}

/// Determine the cursor shape based on current interaction state and hover position.
pub fn determine_cursor_shape(
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
