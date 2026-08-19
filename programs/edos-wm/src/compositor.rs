//! Window compositing for the window manager.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use edos_lib::shm::{PROT_READ, shm_map, shm_size, shm_unmap};
use edos_render::font::{self, Weight};
use edos_render::graphics::{Color, Screen};
use edos_render::icons;
use edos_render::image;
use edos_render::text::Style;
use edos_render::theme::{Theme, lerp_color};
use edos_render::window::{WindowListEntry, flags::FLAG_DOCK};

use crate::cursor::Cursor;
use crate::decorations::{
    self, ACCENT_HEIGHT, BORDER_WIDTH, SHADOW_SIZE, TITLE_HEIGHT, TITLE_PADDING, TITLE_TEXT_HEIGHT,
};

/// Draw a monochrome icon mask onto the screen, one pixel at a time.
///
/// Small enough that a per-pixel write costs nothing next to the rest of a
/// frame, and it keeps the mask format the same one the panel uses.
fn draw_icon(screen: &mut Screen, x: i64, y: i64, mask: &icons::Mask, color: Color) {
    for (row, bits) in mask.iter().enumerate() {
        for col in 0..icons::SIZE {
            if bits & (1 << (icons::SIZE - 1 - col)) == 0 {
                continue;
            }
            let _ = screen.draw_rect(
                (x + col as i64) as u64,
                (y + row as i64) as u64,
                1,
                1,
                color,
            );
        }
    }
}

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

    /// Map a window's buffer and report the dimensions it is safe to read at.
    ///
    /// `width` and `height` are the client's own, from `buffer_width` and
    /// `buffer_height`. They are not the window's: a resize changes those the
    /// moment the manager decides it, while the client allocates its matching
    /// buffer some frames later, and reading a buffer at a width it was never
    /// written with produces a sheared picture rather than merely a stale one.
    ///
    /// Still clamped against the mapping's real size, since the dimensions
    /// arrive from another process and only the allocation bounds the read.
    pub fn get_or_map(
        &mut self,
        shm_id: u64,
        width: u32,
        height: u32,
    ) -> Option<(*mut u8, u32, u32)> {
        if let Some(&(ptr, cached_w, cached_h)) = self.mappings.get(&shm_id) {
            return Some((ptr, cached_w, cached_h));
        }

        let ptr = shm_map(shm_id, PROT_READ).ok()?;
        let (safe_w, safe_h) = match shm_size(shm_id) {
            Ok(size) if width > 0 => {
                let rows = (size / 4) / width as usize;
                (width, height.min(rows as u32))
            }
            Ok(_) => (0, 0),
            Err(_) => (width, height),
        };
        self.mappings.insert(shm_id, (ptr, safe_w, safe_h));
        (safe_w > 0 && safe_h > 0).then_some((ptr, safe_w, safe_h))
    }

    /// Drop mappings for windows that are gone.
    ///
    /// Once a frame, not once a region: compositing runs per dirty rectangle
    /// and unmapping between two of them would drop a buffer the next
    /// rectangle still has to read.
    pub fn retain_active(&mut self, windows: &[WindowListEntry]) {
        let active: Vec<u64> = windows
            .iter()
            .filter(|w| w.on_screen() && w.buffer_shm_id != 0)
            .map(|w| w.buffer_shm_id)
            .collect();
        self.cleanup(&active);
    }

    /// Remove mappings for shm_ids that are no longer in use.
    ///
    /// A cached SHM is only removed if it is absent from BOTH the current
    /// and previous frame's active sets.  This prevents the compositor from
    /// unmapping/remapping the "back buffer" of double-buffered windows
    /// every frame (the visible `buffer_shm_id` alternates between two ids).
    fn cleanup(&mut self, active_shm_ids: &[u64]) {
        let stale: Vec<u64> = self
            .mappings
            .keys()
            .filter(|id| !active_shm_ids.contains(id) && !self.prev_active.contains(id))
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

/// Ordered-dither offsets, in sixteenths of one RGB step. The desktop spans
/// around twenty units from its brightest point to its darkest, so rounding
/// every pixel to the nearest step draws visible contour rings; offsetting by
/// less than a step scatters each boundary instead.
const DITHER: [u32; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];

/// Interpolate `a`..`b` and floor the result with a sub-step offset `d` of
/// sixteenths, which is what keeps the desktop free of contour rings.
fn mix_dithered(a: Color, b: Color, t: u8, d: u32) -> u32 {
    let chan = |av: u8, bv: u8| {
        let exact = av as u32 * (255 - t as u32) + bv as u32 * t as u32;
        ((exact * 16 + d * 255) / (255 * 16)) as u8
    };
    Color::from_rgb(
        chan(a.red(), b.red()),
        chan(a.green(), b.green()),
        chan(a.blue(), b.blue()),
    )
    .raw()
}

/// How many generated grounds there are, one per light position.
pub const LIT_GROUNDS: usize = 3;

/// Where wallpapers are installed. Every readable BMP in it becomes one more
/// background to cycle through.
pub const WALLPAPER_DIR: &str = "/share/wallpapers";

/// Where the desktop ground comes from.
///
/// The generated grounds come first and one of them is always available, so a
/// machine with no wallpapers installed behaves exactly as it did before there
/// was a decoder, and an unreadable image costs its own entry rather than the
/// desktop.
#[derive(Clone, Debug)]
pub enum Background {
    /// A lit ground, by light position.
    Lit(usize),
    /// An image file, scaled to cover the screen.
    Wallpaper(PathBuf),
}

/// Every background the user can cycle through, generated ones first.
pub fn available_backgrounds() -> Vec<Background> {
    let mut all: Vec<Background> = (0..LIT_GROUNDS).map(Background::Lit).collect();
    let Ok(entries) = fs::read_dir(WALLPAPER_DIR) else {
        return all;
    };
    let mut wallpapers: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("bmp"))
        })
        .collect();
    // By name, so the order a user sees does not depend on directory order.
    wallpapers.sort();
    all.extend(wallpapers.into_iter().map(Background::Wallpaper));
    all
}

/// How a background is recorded in `/etc/wallpaper`: a generated ground as
/// `lit:N`, an image as its path.
///
/// The path rather than an index, so the recorded choice survives a wallpaper
/// being added to or removed from [`WALLPAPER_DIR`], and so the file can be
/// set by hand with `echo`.
pub fn background_name(background: &Background) -> String {
    match background {
        Background::Lit(variant) => format!("lit:{variant}"),
        Background::Wallpaper(path) => path.display().to_string(),
    }
}

/// Which of `backgrounds` the recorded `name` selects, or None if it names one
/// that is no longer there.
pub fn background_index(backgrounds: &[Background], name: &str) -> Option<usize> {
    backgrounds
        .iter()
        .position(|background| background_name(background) == name)
}

/// Render `background` at screen size, falling back to the first lit ground if
/// the image cannot be read.
pub fn build_desktop_cache(width: usize, height: usize, background: &Background) -> Vec<u32> {
    let path = match background {
        Background::Lit(variant) => return build_lit_ground(width, height, *variant),
        Background::Wallpaper(path) => path,
    };

    let decoded = fs::read(path)
        .map_err(|e| format!("{e}"))
        .and_then(|bytes| image::decode_bmp(&bytes).map_err(|e| format!("{e:?}")));
    match decoded {
        Ok(image) => image.scaled_to_cover(width as u32, height as u32),
        Err(reason) => {
            eprintln!("[wm] {}: {reason}", path.display());
            build_lit_ground(width, height, 0)
        }
    }
}

/// Pre-compute a lit ground, one colour per pixel.
///
/// A vertical ramp gives the ground a direction, and a radial falloff over it
/// puts the light above the middle of the screen: the ground is brightest where
/// windows open and darkest at the frame, so a window reads as an object
/// resting on it rather than a panel butted against the same flat fill.
/// `variant` moves the light, which changes the room without needing a file on
/// disk or a decoder to read it.
fn build_lit_ground(width: usize, height: usize, variant: usize) -> Vec<u32> {
    let theme = &Theme::DEFAULT;
    let mut buf = vec![0u32; width * height];
    // Light overhead, from the left, or from the right.
    let cx = match variant % LIT_GROUNDS {
        1 => width as i64 / 4,
        2 => width as i64 * 3 / 4,
        _ => width as i64 / 2,
    };
    let cy = height as i64 * 38 / 100;
    let far_x = cx.max(width as i64 - 1 - cx);
    let far_y = cy.max(height as i64 - 1 - cy);
    let max_d2 = (far_x * far_x + far_y * far_y).max(1);
    let last_row = (height as i64 - 1).max(1);

    for y in 0..height {
        let dy = y as i64 - cy;
        let ramp = lerp_color(
            theme.desktop_bg_top,
            theme.desktop_bg_bottom,
            (y as i64 * 255 / last_row) as u8,
        );
        for (x, px) in buf[y * width..(y + 1) * width].iter_mut().enumerate() {
            let dx = x as i64 - cx;
            // 0 under the light, 255 at the corner furthest from it. Squared
            // distance, so the falloff is gentle across the middle of the
            // screen and gathers into the outer band.
            let d2 = ((dx * dx + dy * dy) * 255 / max_d2) as u32;
            let d = DITHER[(y & 3) * 4 + (x & 3)];
            *px = if d2 < 128 {
                mix_dithered(theme.desktop_glow, ramp, (d2 * 2) as u8, d)
            } else {
                mix_dithered(ramp, theme.desktop_edge, ((d2 - 128) * 2) as u8, d)
            };
        }
    }
    buf
}

/// Composite all visible windows onto the screen.
pub fn composite(
    screen: &mut Screen,
    windows: &[WindowListEntry],
    cursor: &Cursor,
    focused_id: Option<u64>,
    shm_cache: &mut ShmCache,
    hovered_close_window: Option<u64>,
    hw_cursor: bool,
    desktop_cache: &[u32],
) {
    // Blit the cached desktop background, over the clip only: this is a
    // megapixel copy, and doing it whole for a region the size of a text row is
    // most of what compositing a small change used to cost.
    let screen_w = screen.width();
    let (cx0, cy0, cx1, cy1) = screen.clip_bounds();
    if let Some((pixels, stride)) = screen.pixels_mut() {
        let x1 = cx1.min(stride).min(screen_w);
        for row in cy0..cy1 {
            if cx0 >= x1 {
                break;
            }
            let src_start = row * screen_w + cx0;
            let dst_start = row * stride + cx0;
            let span = x1 - cx0;
            if src_start + span <= desktop_cache.len() {
                pixels[dst_start..dst_start + span]
                    .copy_from_slice(&desktop_cache[src_start..src_start + span]);
            }
        }
    }

    // Draw windows back-to-front (already sorted by z_order from kernel)
    for window in windows.iter() {
        // A minimized window stays in the list so the taskbar can offer a way
        // back, but it occupies no screen space.
        if window.on_screen() {
            draw_window_direct(
                screen,
                window,
                focused_id == Some(window.id),
                shm_cache,
                hovered_close_window,
            );
        }
    }

    // Draw cursor on top (skip if hardware cursor is active)
    if !hw_cursor {
        draw_cursor(screen, cursor);
    }
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

    // --- Focus accent ---
    // The focused window is the only thing on screen wearing the accent colour,
    // so which window has the keyboard is readable without comparing shades.
    if is_focused {
        draw_clipped_rect(
            screen,
            bw,
            bw,
            w,
            ACCENT_HEIGHT,
            Theme::DEFAULT.title_accent,
        );
    }

    // --- Title text ---
    let title = window.title_str();
    if !title.is_empty() {
        let text_x = window.x as i64 + bw + TITLE_PADDING;
        let text_y = window.y as i64 + bw + (title_bar_h - TITLE_TEXT_HEIGHT).max(0) / 2;
        if text_x >= 0 && text_y >= 0 && text_x < screen_w && text_y < screen_h {
            let color = if is_focused {
                Theme::DEFAULT.title_text
            } else {
                Theme::DEFAULT.title_text_inactive
            };
            // The focused window's title carries weight as well as colour, so
            // focus reads without relying on a hairline the eye has to hunt for.
            let weight = if is_focused {
                Weight::Semibold
            } else {
                Weight::Regular
            };
            let style = Style::new(color.raw())
                .with_weight(weight)
                .with_px(font::size::TITLE);
            let _ = screen.draw_styled_text(text_x as i32, text_y as i32, title, style);
        }
    }

    // --- Title bar buttons: minimize, maximize, close ---
    // Right to left, so the destructive one is furthest from where the hand
    // rests after dragging the bar.
    let btn_size = decorations::CLOSE_BUTTON_SIZE as i64;
    let btn_ry = decorations::button_y();
    let close_hovered = hovered_close_window == Some(window.id);

    for index in [
        decorations::BUTTON_MINIMIZE,
        decorations::BUTTON_MAXIMIZE,
        decorations::BUTTON_CLOSE,
    ] {
        let bx = decorations::button_x(window.width, index);
        let is_close = index == decorations::BUTTON_CLOSE;

        // Closing is destructive and rarely wanted, so it stays quiet until the
        // pointer is on it: a muted glyph at rest, a red field under the cursor.
        if is_close && close_hovered {
            draw_clipped_rect(
                screen,
                bx,
                btn_ry,
                btn_size,
                btn_size,
                Theme::DEFAULT.close_button_hover,
            );
            for &corner_row in &[0i64, btn_size - 1] {
                let title_row = (btn_ry - bw) + corner_row;
                let t_corner = ((title_row * 255) / (title_bar_h - 1).max(1)).min(255) as u8;
                let corner_color =
                    edos_render::theme::lerp_color(title_top, title_bottom, t_corner);
                draw_clipped_rect(screen, bx, btn_ry + corner_row, 1, 1, corner_color);
                draw_clipped_rect(
                    screen,
                    bx + btn_size - 1,
                    btn_ry + corner_row,
                    1,
                    1,
                    corner_color,
                );
            }
        }

        let glyph = if is_close && close_hovered {
            Theme::DEFAULT.close_glyph_hover
        } else {
            Theme::DEFAULT.close_glyph
        };
        let abs_x = window.x as i64 + bx;
        let abs_y = window.y as i64 + btn_ry;
        if abs_x < 0 || abs_y < 0 || abs_x + btn_size > screen_w || abs_y + btn_size > screen_h {
            continue;
        }
        if is_close {
            let gx = abs_x + (btn_size - 10) / 2;
            let gy = abs_y + (btn_size - 10) / 2;
            draw_close_x(screen, gx as u64, gy as u64, glyph);
        } else {
            let mask = if index == decorations::BUTTON_MAXIMIZE {
                &icons::MAXIMIZE
            } else {
                &icons::MINIMIZE
            };
            let gx = abs_x + (btn_size - icons::SIZE as i64) / 2;
            let gy = abs_y + (btn_size - icons::SIZE as i64) / 2;
            draw_icon(screen, gx, gy, mask, glyph);
        }
    }

    // --- Content area background ---
    draw_clipped_rect(screen, bw, th, w, h, Theme::DEFAULT.background);

    // --- Drop shadow (alpha-blended, fades out over SHADOW_SIZE pixels) ---
    {
        let shadow_layers = SHADOW_SIZE as i64;
        let base_alpha = 80u32; // starting opacity (out of 255)

        let blend_shadow_rect =
            |screen: &mut Screen, rx: i64, ry: i64, rw: i64, rh: i64, alpha: u32| {
                let abs_x = window.x as i64 + rx;
                let abs_y = window.y as i64 + ry;
                if abs_x + rw <= 0 || abs_y + rh <= 0 || abs_x >= screen_w || abs_y >= screen_h {
                    return;
                }
                let (cx0, cy0, cx1, cy1) = screen.clip_bounds();
                let x0 = (abs_x.max(0) as u64).max(cx0 as u64);
                let y0 = (abs_y.max(0) as u64).max(cy0 as u64);
                let x1 = ((abs_x + rw) as u64).min(screen_w as u64).min(cx1 as u64);
                let y1 = ((abs_y + rh) as u64).min(screen_h as u64).min(cy1 as u64);
                if let Some((pixels, stride)) = screen.pixels_mut() {
                    let inv = 255 - alpha;
                    for py in y0..y1 {
                        for px in x0..x1 {
                            let idx = py as usize * stride + px as usize;
                            let dst = pixels[idx];
                            let dr = (dst >> 16) & 0xFF;
                            let dg = (dst >> 8) & 0xFF;
                            let db = dst & 0xFF;
                            // Blend toward black (shadow color)
                            let r = (dr * inv) / 255;
                            let g = (dg * inv) / 255;
                            let b = (db * inv) / 255;
                            pixels[idx] = 0xFF000000 | (r << 16) | (g << 8) | b;
                        }
                    }
                }
            };

        for i in 0..shadow_layers {
            let alpha = base_alpha * (shadow_layers - i) as u32 / shadow_layers as u32;
            // Right edge
            blend_shadow_rect(screen, total_w + i, i + 1, 1, total_h - i, alpha);
            // Bottom edge
            blend_shadow_rect(screen, i + 1, total_h + i, total_w - i, 1, alpha);
        }
    }

    // Blit client buffer content with clipping
    if window.buffer_shm_id != 0
        && let Some((ptr, buf_w, buf_h)) = shm_cache.get_or_map(
            window.buffer_shm_id,
            window.buffer_width,
            window.buffer_height,
        )
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

            // Calculate visible dimensions of the content.
            //
            // Bounded by the window as well as by the buffer: the two
            // disagree for the frames between a resize and the client
            // allocating to match, and a buffer wider than its window
            // paints over the ground the window just vacated -- which is
            // then stranded there, because nothing marks it dirty again
            // once the client does catch up.
            let vis_w = ((buf_w as u64).saturating_sub(src_off_x))
                .min(screen_w as u64 - dst_x)
                .min(window.width as u64);
            let vis_h = ((buf_h as u64).saturating_sub(src_off_y))
                .min(screen_h as u64 - dst_y)
                .min(window.height as u64);

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
    if window.buffer_shm_id != 0
        && let Some((ptr, buf_w, buf_h)) = shm_cache.get_or_map(
            window.buffer_shm_id,
            window.buffer_width,
            window.buffer_height,
        )
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

            // Calculate visible dimensions of the content.
            //
            // Bounded by the window as well as by the buffer: the two
            // disagree for the frames between a resize and the client
            // allocating to match, and a buffer wider than its window
            // paints over the ground the window just vacated -- which is
            // then stranded there, because nothing marks it dirty again
            // once the client does catch up.
            let vis_w = ((buf_w as u64).saturating_sub(src_off_x))
                .min(screen_w as u64 - dst_x)
                .min(window.width as u64);
            let vis_h = ((buf_h as u64).saturating_sub(src_off_y))
                .min(screen_h as u64 - dst_y)
                .min(window.height as u64);

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
