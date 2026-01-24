//! Window compositing for the window manager.

use std::collections::HashMap;

use edos_render::graphics::{Color, Screen};
use edos_render::window::{shm_map, shm_unmap, WindowListEntry, PROT_READ};

use crate::cursor::Cursor;
use crate::decorations::{self, BORDER_WIDTH, TITLE_HEIGHT};

/// Cache for shared memory mappings to avoid map/unmap on every frame.
pub struct ShmCache {
    /// Maps shm_id to (mapped_ptr, original_width, original_height).
    mappings: HashMap<u64, (*mut u8, u32, u32)>,
}

impl ShmCache {
    pub fn new() -> Self {
        Self {
            mappings: HashMap::new(),
        }
    }

    /// Get or create a mapping for the given shm_id.
    /// Returns the mapped pointer and the original buffer dimensions if successful.
    /// The returned dimensions are from when the buffer was first mapped,
    /// which may differ from the current window dimensions if the window was resized.
    pub fn get_or_map(&mut self, shm_id: u64, width: u32, height: u32) -> Option<(*mut u8, u32, u32)> {
        if let Some(&(ptr, orig_w, orig_h)) = self.mappings.get(&shm_id) {
            return Some((ptr, orig_w, orig_h));
        }

        // Not cached, map it
        if let Ok(ptr) = shm_map(shm_id, PROT_READ) {
            self.mappings.insert(shm_id, (ptr, width, height));
            Some((ptr, width, height))
        } else {
            None
        }
    }

    /// Remove mappings for shm_ids that are no longer in use.
    /// Call this with the set of currently active shm_ids.
    pub fn cleanup(&mut self, active_shm_ids: &[u64]) {
        let stale: Vec<u64> = self
            .mappings
            .keys()
            .filter(|id| !active_shm_ids.contains(id))
            .copied()
            .collect();

        for shm_id in stale {
            if let Some((ptr, _, _)) = self.mappings.remove(&shm_id) {
                let _ = shm_unmap(ptr);
            }
        }
    }
}

/// Desktop background color.
pub const DESKTOP_COLOR: Color = Color::from_rgb(0x30, 0x30, 0x40);

/// Title bar color for active windows.
const COLOR_TITLE_ACTIVE: Color = Color::from_rgb(0x40, 0x60, 0x90);

/// Title bar color for inactive windows.
const COLOR_TITLE_INACTIVE: Color = Color::from_rgb(0x50, 0x50, 0x60);

/// Border color.
const COLOR_BORDER: Color = Color::from_rgb(0x20, 0x20, 0x20);

/// Close button color.
const COLOR_CLOSE_BUTTON: Color = Color::from_rgb(0xE0, 0x40, 0x40);

/// Composite all visible windows onto the screen.
pub fn composite(
    screen: &mut Screen,
    windows: &[WindowListEntry],
    cursor: &Cursor,
    focused_id: Option<u64>,
    shm_cache: &mut ShmCache,
) {
    // Clear to desktop background
    let _ = screen.fill(DESKTOP_COLOR);

    // Collect active shm_ids for cache cleanup
    let active_shm_ids: Vec<u64> = windows
        .iter()
        .filter(|w| w.visible != 0 && w.buffer_shm_id != 0)
        .map(|w| w.buffer_shm_id)
        .collect();

    // Clean up stale mappings (windows that were destroyed)
    shm_cache.cleanup(&active_shm_ids);

    // Draw windows back-to-front (already sorted by z_order from kernel)
    for window in windows.iter() {
        if window.visible != 0 {
            draw_window_direct(screen, window, focused_id == Some(window.id), shm_cache);
        }
    }

    // Draw cursor on top
    draw_cursor(screen, cursor);
}

/// Draw a single window with decorations directly to the screen buffer.
/// This avoids per-frame allocations by drawing directly.
fn draw_window_direct(screen: &mut Screen, window: &WindowListEntry, is_focused: bool, shm_cache: &mut ShmCache) {
    // Skip windows that are completely off-screen to the left or top
    // (negative coordinates would overflow when cast to u64)
    if window.x < 0 || window.y < 0 {
        return;
    }

    let win_x = window.x as u64;
    let win_y = window.y as u64;
    let w = window.width as u64;
    let h = window.height as u64;
    let total_w = decorations::decorated_width(window.width);
    let total_h = decorations::decorated_height(window.height);

    // Draw border (outer rectangle)
    let _ = screen.draw_rect(win_x, win_y, total_w, total_h, COLOR_BORDER);

    // Draw title bar
    let title_color = if is_focused {
        COLOR_TITLE_ACTIVE
    } else {
        COLOR_TITLE_INACTIVE
    };
    let _ = screen.draw_rect(
        win_x + BORDER_WIDTH,
        win_y + BORDER_WIDTH,
        w,
        TITLE_HEIGHT - BORDER_WIDTH,
        title_color,
    );

    // Draw close button (right side of title bar)
    let close_x = win_x + BORDER_WIDTH + w - 20;
    let close_y = win_y + BORDER_WIDTH + 2;
    let _ = screen.draw_rect(
        close_x,
        close_y,
        18,
        TITLE_HEIGHT - BORDER_WIDTH - 4,
        COLOR_CLOSE_BUTTON,
    );

    // Draw X symbol on close button
    draw_close_x(screen, close_x + 4, close_y + 3);

    // Draw content area background
    let _ = screen.draw_rect(
        win_x + BORDER_WIDTH,
        win_y + TITLE_HEIGHT,
        w,
        h,
        Color::from_rgb(0x30, 0x30, 0x30),
    );

    // Blit client buffer content directly
    if window.buffer_shm_id != 0 {
        if let Some((ptr, buf_w, buf_h)) = shm_cache.get_or_map(window.buffer_shm_id, window.width, window.height) {
            // Use the original buffer dimensions to avoid reading past the buffer
            // This handles the case where window was resized but client hasn't reallocated its buffer
            let pixel_count = (buf_w as usize) * (buf_h as usize);
            let pixels = unsafe { std::slice::from_raw_parts(ptr as *const u32, pixel_count) };

            // Draw pixels directly to screen at content offset
            let content_x = win_x + BORDER_WIDTH;
            let content_y = win_y + TITLE_HEIGHT;

            // Blit using the original buffer dimensions (safe to read)
            let _ = screen.blit_pixels_direct(
                pixels,
                buf_w as u64,
                buf_h as u64,
                content_x,
                content_y,
            );
        }
    }
}

/// Draw the cursor.
fn draw_cursor(screen: &mut Screen, cursor: &Cursor) {
    let cx = cursor.x.max(0) as u64;
    let cy = cursor.y.max(0) as u64;
    let _ = screen.draw_texture_transparent(&cursor.texture, cx, cy);
}

/// Draw an X symbol for the close button.
fn draw_close_x(screen: &mut Screen, x: u64, y: u64) {
    let color = Color::WHITE;
    // Draw a 10x10 X
    for i in 0..10u64 {
        // Main diagonal (top-left to bottom-right)
        let _ = screen.set_pixel(x + i, y + i, color);
        // Anti-diagonal (top-right to bottom-left)
        let _ = screen.set_pixel(x + 9 - i, y + i, color);
        // Thicker lines
        if i > 0 {
            let _ = screen.set_pixel(x + i - 1, y + i, color);
            let _ = screen.set_pixel(x + 9 - i + 1, y + i, color);
        }
    }
}

/// Simple frame buffer clear (used for partial updates in future).
#[allow(dead_code)]
pub fn clear_region(screen: &mut Screen, x: u64, y: u64, width: u64, height: u64) {
    let _ = screen.draw_rect(x, y, width, height, DESKTOP_COLOR);
}
