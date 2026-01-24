//! Window decorations for the window manager.

use edos_render::graphics::{Color, DrawRequest};
use edos_render::window::WindowListEntry;

/// Height of the title bar.
pub const TITLE_HEIGHT: u64 = 24;

/// Width of the window border.
pub const BORDER_WIDTH: u64 = 2;

/// Title bar color for active windows.
pub const COLOR_TITLE_ACTIVE: Color = Color::from_rgb(0x40, 0x60, 0x90);

/// Title bar color for inactive windows.
pub const COLOR_TITLE_INACTIVE: Color = Color::from_rgb(0x50, 0x50, 0x60);

/// Border color.
pub const COLOR_BORDER: Color = Color::from_rgb(0x20, 0x20, 0x20);

/// Close button color.
pub const COLOR_CLOSE_BUTTON: Color = Color::from_rgb(0xE0, 0x40, 0x40);

/// Close button hover color.
pub const COLOR_CLOSE_BUTTON_HOVER: Color = Color::from_rgb(0xFF, 0x60, 0x60);

/// Calculate the total decorated window width.
pub fn decorated_width(window_width: u32) -> u64 {
    window_width as u64 + BORDER_WIDTH * 2
}

/// Calculate the total decorated window height.
pub fn decorated_height(window_height: u32) -> u64 {
    window_height as u64 + TITLE_HEIGHT + BORDER_WIDTH
}

/// Calculate the content area position within the decorated frame.
pub fn content_offset() -> (u64, u64) {
    (BORDER_WIDTH, TITLE_HEIGHT)
}

/// Draw window decorations (title bar, border, close button) to a draw request.
///
/// This draws at position (0, 0) within the request; caller should position the request.
pub fn draw_frame(
    request: &mut DrawRequest,
    window: &WindowListEntry,
    is_focused: bool,
) {
    let w = window.width as u64;
    let h = window.height as u64;
    let total_w = decorated_width(window.width);
    let total_h = decorated_height(window.height);

    // Draw border (outer rectangle)
    let _ = request.fill_rect(0, 0, total_w, total_h, COLOR_BORDER);

    // Draw title bar
    let title_color = if is_focused {
        COLOR_TITLE_ACTIVE
    } else {
        COLOR_TITLE_INACTIVE
    };
    let _ = request.fill_rect(BORDER_WIDTH, BORDER_WIDTH, w, TITLE_HEIGHT - BORDER_WIDTH, title_color);

    // Draw close button (right side of title bar)
    let close_x = BORDER_WIDTH + w - 20;
    let close_y = BORDER_WIDTH + 2;
    let _ = request.fill_rect(close_x, close_y, 18, TITLE_HEIGHT - BORDER_WIDTH - 4, COLOR_CLOSE_BUTTON);

    // Draw X on close button
    draw_close_x(request, close_x + 4, close_y + 3);

    // Draw content area background (will be overwritten by window content)
    let _ = request.fill_rect(
        BORDER_WIDTH,
        TITLE_HEIGHT,
        w,
        h,
        Color::from_rgb(0x30, 0x30, 0x30),
    );
}

/// Draw an X symbol for the close button.
fn draw_close_x(request: &mut DrawRequest, x: u64, y: u64) {
    let color = Color::WHITE;
    // Simple X pattern (10x10)
    for i in 0..10 {
        let _ = request.set_pixel(x + i, y + i, color);
        let _ = request.set_pixel(x + 9 - i, y + i, color);
        // Make it thicker
        if i > 0 {
            let _ = request.set_pixel(x + i - 1, y + i, color);
            let _ = request.set_pixel(x + 9 - i + 1, y + i, color);
        }
    }
}

/// Check if a point is within the close button area of a window.
pub fn is_in_close_button(window: &WindowListEntry, screen_x: i32, screen_y: i32) -> bool {
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
pub fn is_in_title_bar(window: &WindowListEntry, screen_x: i32, screen_y: i32) -> bool {
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
