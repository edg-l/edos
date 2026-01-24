//! Window decorations for the window manager.

use edos_render::window::{flags::FLAG_DOCK, WindowListEntry};

/// Height of the title bar.
pub const TITLE_HEIGHT: u64 = 24;

/// Width of the window border.
pub const BORDER_WIDTH: u64 = 2;

/// Size of the resize grab zone in pixels.
pub const RESIZE_BORDER: i64 = 8;

/// Hit regions for mouse interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitRegion {
    None,
    TitleBar,
    CloseButton,
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

/// Calculate the total width for a window considering its flags.
pub fn effective_width(window: &WindowListEntry) -> u64 {
    if (window.flags & FLAG_DOCK) != 0 {
        window.width as u64
    } else {
        decorated_width(window.width)
    }
}

/// Calculate the total height for a window considering its flags.
pub fn effective_height(window: &WindowListEntry) -> u64 {
    if (window.flags & FLAG_DOCK) != 0 {
        window.height as u64
    } else {
        decorated_height(window.height)
    }
}

/// Check if a point is within the close button area of a window.
fn is_in_close_button(window: &WindowListEntry, screen_x: i32, screen_y: i32) -> bool {
    let win_x = window.x as i64;
    let win_y = window.y as i64;
    let w = window.width as i64;

    let close_x = win_x + BORDER_WIDTH as i64 + w - 20;
    let close_y = win_y + BORDER_WIDTH as i64 + 2;
    let close_w = 18i64;
    let close_h = TITLE_HEIGHT as i64 - BORDER_WIDTH as i64 - 4;

    let px = screen_x as i64;
    let py = screen_y as i64;

    px >= close_x && px < close_x + close_w && py >= close_y && py < close_y + close_h
}

/// Check if a point is within the title bar (for dragging).
fn is_in_title_bar(window: &WindowListEntry, screen_x: i32, screen_y: i32) -> bool {
    let win_x = window.x as i64;
    let win_y = window.y as i64;
    let w = window.width as i64;

    let title_x = win_x + BORDER_WIDTH as i64;
    let title_y = win_y + BORDER_WIDTH as i64;
    let title_w = w - 20; // Exclude close button area
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

    // Check close button first (has highest priority in title area)
    if is_in_close_button(window, screen_x, screen_y) {
        return HitRegion::CloseButton;
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
