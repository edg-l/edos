//! Window compositing for the window manager.

use std::collections::HashMap;

use edos_render::graphics::{Color, RasterHeight, Screen, TextStyle};
use edos_render::theme::Theme;
use edos_render::window::{PROT_READ, WindowListEntry, flags::FLAG_DOCK, shm_map, shm_unmap};

use crate::cursor::Cursor;
use crate::decorations::{self, BORDER_WIDTH, SHADOW_SIZE, TITLE_HEIGHT};

/// Cache for shared memory mappings to avoid map/unmap on every frame.
pub struct ShmCache {
    /// Maps shm_id to (mapped_ptr, original_width, original_height).
    mappings: HashMap<u64, (*mut u8, u32, u32)>,
    /// SHM ids that were active in the previous frame.
    /// Kept to avoid unmapping the "back buffer" of double-buffered windows,
    /// since `buffer_shm_id` alternates between two ids each frame.
    prev_active: Vec<u64>,
}

impl ShmCache {
    pub fn new() -> Self {
        Self {
            mappings: HashMap::new(),
            prev_active: Vec::new(),
        }
    }

    /// Get or create a mapping for the given shm_id.
    /// Returns the mapped pointer and buffer dimensions if successful.
    ///
    /// IMPORTANT: A shared memory buffer's size is fixed at creation time.
    /// If the kernel reports different dimensions than we cached, it means
    /// the WM updated the window size but the client hasn't created a new
    /// buffer yet. We return the cached (actual) dimensions to avoid
    /// reading beyond the buffer's real size. Once the client creates a
    /// new shm_id, cleanup() will remove the old mapping.
    pub fn get_or_map(
        &mut self,
        shm_id: u64,
        width: u32,
        height: u32,
    ) -> Option<(*mut u8, u32, u32)> {
        if let Some(&(ptr, cached_w, cached_h)) = self.mappings.get(&shm_id) {
            return Some((ptr, cached_w, cached_h));
        }

        // Not cached, map fresh
        if let Ok(ptr) = shm_map(shm_id, PROT_READ) {
            self.mappings.insert(shm_id, (ptr, width, height));
            Some((ptr, width, height))
        } else {
            None
        }
    }

    /// Remove mappings for shm_ids that are no longer in use.
    ///
    /// A cached SHM is only removed if it is absent from BOTH the current
    /// and previous frame's active sets.  This prevents the compositor from
    /// unmapping/remapping the "back buffer" of double-buffered windows
    /// every frame (the visible `buffer_shm_id` alternates between two ids).
    pub fn cleanup(&mut self, active_shm_ids: &[u64]) {
        let stale: Vec<u64> = self
            .mappings
            .keys()
            .filter(|id| {
                !active_shm_ids.contains(id) && !self.prev_active.contains(id)
            })
            .copied()
            .collect();

        for shm_id in stale {
            if let Some((ptr, _, _)) = self.mappings.remove(&shm_id) {
                let _ = shm_unmap(ptr);
            }
        }

        self.prev_active = active_shm_ids.to_vec();
    }
}

/// Composite all visible windows onto the screen.
pub fn composite(
    screen: &mut Screen,
    windows: &[WindowListEntry],
    cursor: &Cursor,
    focused_id: Option<u64>,
    shm_cache: &mut ShmCache,
    hovered_close_window: Option<u64>,
) {
    // Draw desktop background gradient
    edos_render::theme::draw_gradient_v_screen(
        screen,
        0,
        0,
        screen.width() as u64,
        screen.height() as u64,
        Theme::DEFAULT.desktop_bg_top,
        Theme::DEFAULT.desktop_bg_bottom,
    );

    // Collect active shm_ids for cache cleanup
    let active_shm_ids: Vec<u64> = windows
        .iter()
        .filter(|w| w.visible != 0 && w.buffer_shm_id != 0)
        .map(|w| w.buffer_shm_id)
        .collect();

    // Clean up stale mappings (windows that were destroyed)
    shm_cache.cleanup(&active_shm_ids);

    // Draw windows back-to-front (already sorted by z_order from kernel)
    for (_i, window) in windows.iter().enumerate() {
        if window.visible != 0 {
            draw_window_direct(
                screen,
                window,
                focused_id == Some(window.id),
                shm_cache,
                hovered_close_window,
            );
        }
    }

    // Draw cursor on top
    draw_cursor(screen, cursor);
}

/// Draw a single window with decorations directly to the screen buffer.
/// This avoids per-frame allocations by drawing directly.
/// Handles windows partially off-screen by clipping to visible region.
fn draw_window_direct(
    screen: &mut Screen,
    window: &WindowListEntry,
    is_focused: bool,
    shm_cache: &mut ShmCache,
    hovered_close_window: Option<u64>,
) {
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
    let draw_clipped_rect =
        |screen: &mut Screen, rx: i64, ry: i64, rw: i64, rh: i64, color: Color| {
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

    let bw = BORDER_WIDTH as i64;
    let th = TITLE_HEIGHT as i64;

    // --- Flat border ---
    let border_color = if is_focused {
        Theme::DEFAULT.window_border_highlight
    } else {
        Theme::DEFAULT.window_border_shadow
    };
    draw_clipped_rect(screen, 0, 0, total_w, bw, border_color); // top
    draw_clipped_rect(screen, 0, total_h - bw, total_w, bw, border_color); // bottom
    draw_clipped_rect(screen, 0, bw, bw, total_h - 2 * bw, border_color); // left
    draw_clipped_rect(screen, total_w - bw, bw, bw, total_h - 2 * bw, border_color); // right

    // --- Gradient title bar ---
    let (title_top, title_bottom) = if is_focused {
        (
            Theme::DEFAULT.title_active_top,
            Theme::DEFAULT.title_active_bottom,
        )
    } else {
        (
            Theme::DEFAULT.title_inactive_top,
            Theme::DEFAULT.title_inactive_bottom,
        )
    };
    let title_bar_h = th - bw;
    for row in 0..title_bar_h {
        let t = ((row * 255) / (title_bar_h - 1).max(1)) as u8;
        let color = edos_render::theme::lerp_color(title_top, title_bottom, t);
        draw_clipped_rect(screen, bw, bw + row, w, 1, color);
    }

    // --- Title text ---
    let title = window.title_str();
    if !title.is_empty() {
        let text_x = window.x as i64 + bw + 6;
        let text_y = window.y as i64 + bw + 3;
        if text_x >= 0 && text_y >= 0 && text_x < screen_w && text_y < screen_h {
            let style =
                TextStyle::new(Color::from_rgb(0xFF, 0xFF, 0xFF)).with_size(RasterHeight::Size16);
            let _ = screen.draw_text(text_x as u64, text_y as u64, title, &style);
        }
    }

    // --- Close button (rounded corners) ---
    // Positioned from the right border, vertically centered in the title bar.
    let btn_size = decorations::CLOSE_BUTTON_SIZE as i64;
    let close_rx = bw + w - decorations::CLOSE_BUTTON_MARGIN as i64 - btn_size;
    let close_ry = bw + (title_bar_h - btn_size) / 2;

    let btn_color = if hovered_close_window == Some(window.id) {
        Theme::DEFAULT.close_button_hover
    } else {
        Theme::DEFAULT.close_button_normal
    };

    // Draw button background
    draw_clipped_rect(screen, close_rx, close_ry, btn_size, btn_size, btn_color);

    // Cut the 4 corner pixels to simulate rounding (replace with title bar color at that row).
    // Corner rows are 0 and btn_size-1 relative to the button.
    for &corner_row in &[0i64, btn_size - 1] {
        let title_row = (close_ry - bw) + corner_row; // row index within the title gradient
        let t_corner = ((title_row * 255) / (title_bar_h - 1).max(1)).min(255) as u8;
        let corner_color = edos_render::theme::lerp_color(title_top, title_bottom, t_corner);
        draw_clipped_rect(screen, close_rx, close_ry + corner_row, 1, 1, corner_color);
        draw_clipped_rect(
            screen,
            close_rx + btn_size - 1,
            close_ry + corner_row,
            1,
            1,
            corner_color,
        );
    }

    // Draw 10x10 X glyph centered inside the 20x20 button.
    let x_offset_x = close_rx + (btn_size - 10) / 2;
    let x_offset_y = close_ry + (btn_size - 10) / 2;
    let close_abs_x = window.x as i64 + x_offset_x;
    let close_abs_y = window.y as i64 + x_offset_y;
    if close_abs_x >= 0
        && close_abs_y >= 0
        && close_abs_x + 10 <= screen_w
        && close_abs_y + 10 <= screen_h
    {
        draw_close_x(
            screen,
            close_abs_x as u64,
            close_abs_y as u64,
            Theme::DEFAULT.close_button_x,
        );
    }

    // --- Content area background ---
    draw_clipped_rect(screen, bw, th, w, h, Theme::DEFAULT.background);

    // --- Drop shadow (drawn outside the decorated rect) ---
    let shadow_color = Theme::DEFAULT.window_shadow;
    // Right shadow strip: 2px wide, full height of decorated window
    draw_clipped_rect(
        screen,
        total_w,
        0,
        SHADOW_SIZE as i64,
        total_h,
        shadow_color,
    );
    // Bottom shadow strip: full decorated width + shadow width
    draw_clipped_rect(
        screen,
        0,
        total_h,
        total_w + SHADOW_SIZE as i64,
        SHADOW_SIZE as i64,
        shadow_color,
    );

    // Blit client buffer content with clipping
    if window.buffer_shm_id != 0 {
        if let Some((ptr, buf_w, buf_h)) =
            shm_cache.get_or_map(window.buffer_shm_id, window.width, window.height)
        {
            let pixel_count = (buf_w as usize) * (buf_h as usize);
            let pixels = unsafe { std::slice::from_raw_parts(ptr as *const u32, pixel_count) };

            // Content position in window-relative coordinates
            let content_rx = bw;
            let content_ry = th;

            // Absolute screen position of content
            let content_abs_x = window.x as i64 + content_rx;
            let content_abs_y = window.y as i64 + content_ry;

            // Skip if content is completely off-screen
            if content_abs_x + (buf_w as i64) > 0
                && content_abs_y + (buf_h as i64) > 0
                && content_abs_x < screen_w
                && content_abs_y < screen_h
            {
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
        if let Some((ptr, buf_w, buf_h)) =
            shm_cache.get_or_map(window.buffer_shm_id, window.width, window.height)
        {
            let pixel_count = (buf_w as usize) * (buf_h as usize);
            let pixels = unsafe { std::slice::from_raw_parts(ptr as *const u32, pixel_count) };

            // Absolute screen position of content (no decoration offset)
            let content_abs_x = window.x as i64;
            let content_abs_y = window.y as i64;

            // Skip if content is completely off-screen
            if content_abs_x + (buf_w as i64) > 0
                && content_abs_y + (buf_h as i64) > 0
                && content_abs_x < screen_w
                && content_abs_y < screen_h
            {
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
    let _ = screen.draw_texture_transparent(cursor.current_texture(), cx, cy);
}

/// Draw a 10x10 X symbol for the close button with the given color.
fn draw_close_x(screen: &mut Screen, x: u64, y: u64, color: Color) {
    for i in 0..10u64 {
        // Main diagonal
        let _ = screen.set_pixel(x + i, y + i, color);
        // Anti-diagonal
        let _ = screen.set_pixel(x + 9 - i, y + i, color);
        // Thicken: extra pixel on each diagonal
        if i > 0 {
            let _ = screen.set_pixel(x + i - 1, y + i, color);
            let _ = screen.set_pixel(x + 10 - i, y + i, color);
        }
    }
}
