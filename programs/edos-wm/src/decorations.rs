//! Window decorations for the window manager.

use edos_render::window::{WindowListEntry, flags::FLAG_DOCK};

/// Height of the title bar.
///
/// Deep enough for a line of interface type with room above and below, rather
/// than the text cell with four pixels either side.
pub const TITLE_HEIGHT: u64 = 32;

/// Width of the window border.
pub const BORDER_WIDTH: u64 = 1;

/// Height of the accent hairline marking the focused window's title bar.
pub const ACCENT_HEIGHT: i64 = 2;

/// Horizontal padding between the border and the title text.
pub const TITLE_PADDING: i64 = 8;

/// Raster height of the title text, used to centre it in the title bar.
pub const TITLE_TEXT_HEIGHT: i64 = 16;

/// Size of the drop shadow in pixels (drawn outside the decorated area).
pub const SHADOW_SIZE: u64 = 5;

/// Title-bar button size in pixels (square). Close, maximize and minimize all
/// share it, so the group reads as one row.
pub const CLOSE_BUTTON_SIZE: u64 = 24;

/// Margin from the right border to the first title-bar button.
pub const CLOSE_BUTTON_MARGIN: u64 = 4;

/// Gap between adjacent title-bar buttons.
pub const BUTTON_GAP: u64 = 2;

/// Which title-bar button, counting from the right: close is 0.
pub const BUTTON_CLOSE: u64 = 0;
pub const BUTTON_MAXIMIZE: u64 = 1;
pub const BUTTON_MINIMIZE: u64 = 2;

/// X offset of a title-bar button from the window's left edge.
pub fn button_x(window_width: u32, index: u64) -> i64 {
    let right = BORDER_WIDTH as i64 + window_width as i64 - CLOSE_BUTTON_MARGIN as i64;
    right - ((index + 1) * (CLOSE_BUTTON_SIZE + BUTTON_GAP)) as i64 + BUTTON_GAP as i64
}

/// Y offset of the title-bar buttons, centred in the bar.
pub fn button_y() -> i64 {
    BORDER_WIDTH as i64 + (TITLE_HEIGHT as i64 - CLOSE_BUTTON_SIZE as i64) / 2
}

/// Size of the resize grab zone in pixels.
pub const RESIZE_BORDER: i64 = 8;

/// Hit regions for mouse interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitRegion {
    None,
    TitleBar,
    CloseButton,
    MaximizeButton,
    MinimizeButton,
    Client,
    ResizeTop,
    ResizeBottom,
    ResizeLeft,
    ResizeRight,
    ResizeTopLeft,
    ResizeTopRight,
    ResizeBottomLeft,
    ResizeBottomRight,
}

/// Calculate the total decorated window width.
pub fn decorated_width(window_width: u32) -> u64 {
    window_width as u64 + BORDER_WIDTH * 2
}

/// Calculate the total decorated window height.
pub fn decorated_height(window_height: u32) -> u64 {
    window_height as u64 + TITLE_HEIGHT + BORDER_WIDTH
}

/// Calculate the total width for a window considering its flags (includes shadow).
pub fn effective_width(window: &WindowListEntry) -> u64 {
    if (window.flags & FLAG_DOCK) != 0 {
        window.width as u64
    } else {
        decorated_width(window.width) + SHADOW_SIZE
    }
}

/// Calculate the total height for a window considering its flags (includes shadow).
pub fn effective_height(window: &WindowListEntry) -> u64 {
    if (window.flags & FLAG_DOCK) != 0 {
        window.height as u64
    } else {
        decorated_height(window.height) + SHADOW_SIZE
    }
}

/// Calculate effective width from raw flags and width values (no WindowListEntry needed).
pub fn effective_width_raw(flags: u64, width: u32) -> u64 {
    if (flags & FLAG_DOCK) != 0 {
        width as u64
    } else {
        decorated_width(width) + SHADOW_SIZE
    }
}

/// Calculate effective height from raw flags and height values (no WindowListEntry needed).
pub fn effective_height_raw(flags: u64, height: u32) -> u64 {
    if (flags & FLAG_DOCK) != 0 {
        height as u64
    } else {
        decorated_height(height) + SHADOW_SIZE
    }
}

/// Which title-bar button a point falls on, if any.
pub fn button_at(window: &WindowListEntry, screen_x: i32, screen_y: i32) -> Option<u64> {
    let px = screen_x as i64 - window.x as i64;
    let py = screen_y as i64 - window.y as i64;
    let top = button_y();
    if py < top || py >= top + CLOSE_BUTTON_SIZE as i64 {
        return None;
    }
    [BUTTON_CLOSE, BUTTON_MAXIMIZE, BUTTON_MINIMIZE]
        .into_iter()
        .find(|&index| {
            let left = button_x(window.width, index);
            px >= left && px < left + CLOSE_BUTTON_SIZE as i64
        })
}

/// Check if a point is within the close button area of a window.
fn is_in_close_button(window: &WindowListEntry, screen_x: i32, screen_y: i32) -> bool {
    button_at(window, screen_x, screen_y) == Some(BUTTON_CLOSE)
}

/// Check if a point is within the title bar (for dragging).
fn is_in_title_bar(window: &WindowListEntry, screen_x: i32, screen_y: i32) -> bool {
    let win_x = window.x as i64;
    let win_y = window.y as i64;
    let w = window.width as i64;

    let title_x = win_x + BORDER_WIDTH as i64;
    let title_y = win_y + BORDER_WIDTH as i64;
    let title_w = w - CLOSE_BUTTON_MARGIN as i64 - CLOSE_BUTTON_SIZE as i64;
    let title_h = TITLE_HEIGHT as i64 - BORDER_WIDTH as i64;

    let px = screen_x as i64;
    let py = screen_y as i64;

    px >= title_x && px < title_x + title_w && py >= title_y && py < title_y + title_h
}

/// Unified hit testing function that determines which region of a window the cursor is in.
pub fn hit_test(window: &WindowListEntry, screen_x: i32, screen_y: i32) -> HitRegion {
    // Dock windows only have client area (no decorations)
    if (window.flags & FLAG_DOCK) != 0 {
        let win_x = window.x as i64;
        let win_y = window.y as i64;
        let w = window.width as i64;
        let h = window.height as i64;
        let px = screen_x as i64;
        let py = screen_y as i64;

        if px >= win_x && px < win_x + w && py >= win_y && py < win_y + h {
            return HitRegion::Client;
        }
        return HitRegion::None;
    }

    let win_x = window.x as i64;
    let win_y = window.y as i64;
    let total_w = decorated_width(window.width) as i64;
    let total_h = decorated_height(window.height) as i64;

    let px = screen_x as i64;
    let py = screen_y as i64;

    // Check if point is outside the window entirely
    if px < win_x || px >= win_x + total_w || py < win_y || py >= win_y + total_h {
        return HitRegion::None;
    }

    // Calculate distances from edges
    let from_left = px - win_x;
    let from_right = (win_x + total_w) - px;
    let from_top = py - win_y;
    let from_bottom = (win_y + total_h) - py;

    // Title-bar buttons take priority over dragging the bar they sit in.
    match button_at(window, screen_x, screen_y) {
        Some(BUTTON_CLOSE) => return HitRegion::CloseButton,
        Some(BUTTON_MAXIMIZE) => return HitRegion::MaximizeButton,
        Some(BUTTON_MINIMIZE) => return HitRegion::MinimizeButton,
        _ => {}
    }

    // Check title bar next (clicking title bar should drag, not resize)
    if is_in_title_bar(window, screen_x, screen_y) {
        return HitRegion::TitleBar;
    }

    // Now check resize zones
    let on_left = from_left < RESIZE_BORDER;
    let on_right = from_right <= RESIZE_BORDER;
    let on_bottom = from_bottom <= RESIZE_BORDER;

    // Bottom corners take priority
    if on_bottom && on_left {
        return HitRegion::ResizeBottomLeft;
    }
    if on_bottom && on_right {
        return HitRegion::ResizeBottomRight;
    }

    // Top corners (work from title bar edges too for easier grabbing)
    if from_top < RESIZE_BORDER && on_left {
        return HitRegion::ResizeTopLeft;
    }
    if from_top < RESIZE_BORDER && on_right {
        return HitRegion::ResizeTopRight;
    }

    // Side edges
    if on_left {
        return HitRegion::ResizeLeft;
    }
    if on_right {
        return HitRegion::ResizeRight;
    }

    // Bottom edge
    if on_bottom {
        return HitRegion::ResizeBottom;
    }

    // Top edge only in the thin border area (not title bar)
    if from_top < BORDER_WIDTH as i64 {
        return HitRegion::ResizeTop;
    }

    // Everything else is the client area
    HitRegion::Client
}
