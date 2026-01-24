//! Window compositing for the window manager.

use std::collections::HashMap;

use edos_render::graphics::{Color, Screen};
use edos_render::window::{flags::FLAG_DOCK, shm_map, shm_unmap, WindowListEntry, PROT_READ};

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
/// Handles windows partially off-screen by clipping to visible region.
fn draw_window_direct(screen: &mut Screen, window: &WindowListEntry, is_focused: bool, shm_cache: &mut ShmCache) {
    // Check if this is a dock window (no decorations)
    let is_dock = (window.flags & FLAG_DOCK) != 0;

    if is_dock {
        // Draw dock window directly without decorations
        draw_dock_window(screen, window, shm_cache);
        return;
    }

    let w = window.width as i64;
    let h = window.height as i64;
    let total_w = decorations::decorated_width(window.width) as i64;
    let total_h = decorations::decorated_height(window.height) as i64;
    let screen_w = screen.width() as i64;
    let screen_h = screen.height() as i64;

    // Skip windows completely off-screen
    if window.x + total_w as i32 <= 0 || window.y + total_h as i32 <= 0 {
        return;
    }
    if window.x as i64 >= screen_w || window.y as i64 >= screen_h {
        return;
    }

    // Calculate visible region (clipping)
    let clip_left = (-window.x).max(0) as i64;
    let clip_top = (-window.y).max(0) as i64;
    let draw_x = (window.x).max(0) as u64;
    let draw_y = (window.y).max(0) as u64;

    // Calculate visible dimensions
    let visible_w = ((total_w - clip_left) as u64).min(screen_w as u64 - draw_x);
    let visible_h = ((total_h - clip_top) as u64).min(screen_h as u64 - draw_y);

    if visible_w == 0 || visible_h == 0 {
        return;
    }

    // Helper to draw a rect with clipping applied
    // Takes coordinates relative to window origin (before decoration offset)
    let draw_clipped_rect = |screen: &mut Screen, rx: i64, ry: i64, rw: i64, rh: i64, color: Color| {
        // Convert to absolute screen coordinates
        let abs_x = window.x as i64 + rx;
        let abs_y = window.y as i64 + ry;

        // Skip if completely off-screen
        if abs_x + rw <= 0 || abs_y + rh <= 0 || abs_x >= screen_w || abs_y >= screen_h {
            return;
        }

        // Clip to screen bounds
        let clipped_x = abs_x.max(0) as u64;
        let clipped_y = abs_y.max(0) as u64;
        let clip_l = (-abs_x).max(0) as u64;
        let clip_t = (-abs_y).max(0) as u64;
        let clipped_w = ((rw as u64).saturating_sub(clip_l)).min(screen_w as u64 - clipped_x);
        let clipped_h = ((rh as u64).saturating_sub(clip_t)).min(screen_h as u64 - clipped_y);

        if clipped_w > 0 && clipped_h > 0 {
            let _ = screen.draw_rect(clipped_x, clipped_y, clipped_w, clipped_h, color);
        }
    };

    // Draw border (outer rectangle) - as 4 separate edges for proper clipping
    let bw = BORDER_WIDTH as i64;
    let th = TITLE_HEIGHT as i64;

    // Top edge
    draw_clipped_rect(screen, 0, 0, total_w, bw, COLOR_BORDER);
    // Bottom edge
    draw_clipped_rect(screen, 0, total_h - bw, total_w, bw, COLOR_BORDER);
    // Left edge
    draw_clipped_rect(screen, 0, bw, bw, total_h - 2 * bw, COLOR_BORDER);
    // Right edge
    draw_clipped_rect(screen, total_w - bw, bw, bw, total_h - 2 * bw, COLOR_BORDER);

    // Draw title bar
    let title_color = if is_focused {
        COLOR_TITLE_ACTIVE
    } else {
        COLOR_TITLE_INACTIVE
    };
    draw_clipped_rect(screen, bw, bw, w, th - bw, title_color);

    // Draw close button (right side of title bar)
    let close_rx = bw + w - 20;
    let close_ry = bw + 2;
    draw_clipped_rect(screen, close_rx, close_ry, 18, th - bw - 4, COLOR_CLOSE_BUTTON);

    // Draw X symbol on close button (only if visible)
    let close_abs_x = window.x as i64 + close_rx + 4;
    let close_abs_y = window.y as i64 + close_ry + 3;
    if close_abs_x >= 0 && close_abs_y >= 0 &&
       close_abs_x + 10 <= screen_w && close_abs_y + 10 <= screen_h {
        draw_close_x(screen, close_abs_x as u64, close_abs_y as u64);
    }

    // Draw content area background
    draw_clipped_rect(screen, bw, th, w, h, Color::from_rgb(0x30, 0x30, 0x30));

    // Blit client buffer content with clipping
    if window.buffer_shm_id != 0 {
        if let Some((ptr, buf_w, buf_h)) = shm_cache.get_or_map(window.buffer_shm_id, window.width, window.height) {
            let pixel_count = (buf_w as usize) * (buf_h as usize);
            let pixels = unsafe { std::slice::from_raw_parts(ptr as *const u32, pixel_count) };

            // Content position in window-relative coordinates
            let content_rx = bw;
            let content_ry = th;

            // Absolute screen position of content
            let content_abs_x = window.x as i64 + content_rx;
            let content_abs_y = window.y as i64 + content_ry;

            // Skip if content is completely off-screen
            if content_abs_x + (buf_w as i64) > 0 && content_abs_y + (buf_h as i64) > 0 &&
               content_abs_x < screen_w && content_abs_y < screen_h {
                // Calculate source offset (how much to skip in the buffer)
                let src_off_x = (-content_abs_x).max(0) as u64;
                let src_off_y = (-content_abs_y).max(0) as u64;

                // Calculate destination position (where to draw on screen)
                let dst_x = content_abs_x.max(0) as u64;
                let dst_y = content_abs_y.max(0) as u64;

                // Calculate visible dimensions of the content
                let vis_w = ((buf_w as u64).saturating_sub(src_off_x)).min(screen_w as u64 - dst_x);
                let vis_h = ((buf_h as u64).saturating_sub(src_off_y)).min(screen_h as u64 - dst_y);

                if vis_w > 0 && vis_h > 0 {
                    let _ = screen.blit_pixels_clipped(
                        pixels,
                        buf_w as u64,
                        buf_h as u64,
                        src_off_x,
                        src_off_y,
                        dst_x,
                        dst_y,
                        vis_w,
                        vis_h,
                    );
                }
            }
        }
    }
}

/// Draw a dock window (no decorations, just content).
fn draw_dock_window(screen: &mut Screen, window: &WindowListEntry, shm_cache: &mut ShmCache) {
    let screen_w = screen.width() as i64;
    let screen_h = screen.height() as i64;

    // Skip windows completely off-screen
    if window.x as i64 + window.width as i64 <= 0 || window.y as i64 + window.height as i64 <= 0 {
        return;
    }
    if window.x as i64 >= screen_w || window.y as i64 >= screen_h {
        return;
    }

    // Blit client buffer content with clipping
    if window.buffer_shm_id != 0 {
        if let Some((ptr, buf_w, buf_h)) = shm_cache.get_or_map(window.buffer_shm_id, window.width, window.height) {
            let pixel_count = (buf_w as usize) * (buf_h as usize);
            let pixels = unsafe { std::slice::from_raw_parts(ptr as *const u32, pixel_count) };

            // Absolute screen position of content (no decoration offset)
            let content_abs_x = window.x as i64;
            let content_abs_y = window.y as i64;

            // Skip if content is completely off-screen
            if content_abs_x + (buf_w as i64) > 0 && content_abs_y + (buf_h as i64) > 0 &&
               content_abs_x < screen_w && content_abs_y < screen_h {
                // Calculate source offset (how much to skip in the buffer)
                let src_off_x = (-content_abs_x).max(0) as u64;
                let src_off_y = (-content_abs_y).max(0) as u64;

                // Calculate destination position (where to draw on screen)
                let dst_x = content_abs_x.max(0) as u64;
                let dst_y = content_abs_y.max(0) as u64;

                // Calculate visible dimensions of the content
                let vis_w = ((buf_w as u64).saturating_sub(src_off_x)).min(screen_w as u64 - dst_x);
                let vis_h = ((buf_h as u64).saturating_sub(src_off_y)).min(screen_h as u64 - dst_y);

                if vis_w > 0 && vis_h > 0 {
                    let _ = screen.blit_pixels_clipped(
                        pixels,
                        buf_w as u64,
                        buf_h as u64,
                        src_off_x,
                        src_off_y,
                        dst_x,
                        dst_y,
                        vis_w,
                        vis_h,
                    );
                }
            }
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
