//! Window compositing for the window manager.

use edos_render::graphics::{Color, DrawRequest, Screen, Texture};
use edos_render::window::{shm_map, WindowListEntry, PROT_READ};

use crate::cursor::Cursor;
use crate::decorations::{self, BORDER_WIDTH, TITLE_HEIGHT};

/// Desktop background color.
pub const DESKTOP_COLOR: Color = Color::from_rgb(0x30, 0x30, 0x40);

/// Composite all visible windows onto the screen.
pub fn composite(
    screen: &mut Screen,
    windows: &[WindowListEntry],
    cursor: &Cursor,
    focused_id: Option<u64>,
) {
    // Clear to desktop background
    let _ = screen.fill(DESKTOP_COLOR);

    // Draw windows back-to-front (already sorted by z_order from kernel)
    for window in windows.iter() {
        if window.visible != 0 {
            draw_window(screen, window, focused_id == Some(window.id));
        }
    }

    // Draw cursor on top
    draw_cursor(screen, cursor);
}

/// Draw a single window with decorations.
fn draw_window(screen: &mut Screen, window: &WindowListEntry, is_focused: bool) {
    let total_w = decorations::decorated_width(window.width);
    let total_h = decorations::decorated_height(window.height);

    // Create a draw request for the entire decorated window
    let mut request = match DrawRequest::new(total_w, total_h) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("draw_window: failed to create request: {:?}", e);
            return;
        }
    };

    // Draw window frame (title bar, border, close button)
    decorations::draw_frame(&mut request, window, is_focused);

    // Blit client buffer content
    if window.buffer_shm_id != 0 {
        if let Ok(ptr) = shm_map(window.buffer_shm_id, PROT_READ) {
            let pixel_count = (window.width * window.height) as usize;
            let pixels = unsafe { std::slice::from_raw_parts(ptr as *const u32, pixel_count) };

            // Create texture from window buffer
            if let Ok(texture) =
                Texture::from_buffer(window.width as u64, window.height as u64, pixels.to_vec())
            {
                // Blit to content area (offset by border and title bar)
                let _ = request.blit_texture(&texture, BORDER_WIDTH, TITLE_HEIGHT);
            }

            // Note: We don't unmap here because the compositor needs frequent access
            // In a real implementation, we'd cache the mappings
        }
    }

    // Position and draw the request
    let request = request.with_position(window.x as u64, window.y as u64);

    // Use the framebuffer draw to render the request
    draw_request_to_screen(screen, &request);
}

/// Draw a DrawRequest to the screen.
/// This is a workaround since Screen doesn't expose a direct method for this.
fn draw_request_to_screen(screen: &mut Screen, request: &DrawRequest) {
    // Convert to texture and draw
    if let Ok(texture) =
        Texture::from_buffer(request.width, request.height, request.pixels.clone())
    {
        let _ = screen.draw_texture(&texture, request.x, request.y);
    }
}

/// Draw the cursor.
fn draw_cursor(screen: &mut Screen, cursor: &Cursor) {
    // Draw cursor texture at cursor position
    // Handle cursor being partially off-screen
    let cx = cursor.x.max(0) as u64;
    let cy = cursor.y.max(0) as u64;

    let _ = screen.draw_texture(&cursor.texture, cx, cy);
}

/// Simple frame buffer clear (used for partial updates in future).
pub fn clear_region(screen: &mut Screen, x: u64, y: u64, width: u64, height: u64) {
    let _ = screen.draw_rect(x, y, width, height, DESKTOP_COLOR);
}
