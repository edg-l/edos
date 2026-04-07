//! Theme system providing color constants and drawing helpers for the EDOS GUI.

use crate::graphics::{Color, Screen};

/// Central theme struct holding all color constants for the EDOS GUI.
pub struct Theme {
    // Desktop
    pub desktop_bg_top: Color,
    pub desktop_bg_bottom: Color,

    // Window decorations
    pub title_active_top: Color,
    pub title_active_bottom: Color,
    pub title_inactive_top: Color,
    pub title_inactive_bottom: Color,
    pub window_border_highlight: Color,
    pub window_border_shadow: Color,
    pub window_shadow: Color,
    pub close_button_normal: Color,
    pub close_button_hover: Color,
    pub close_button_x: Color,

    // Taskbar
    pub taskbar_bg_top: Color,
    pub taskbar_bg_bottom: Color,
    pub taskbar_text: Color,
    pub taskbar_button_normal: Color,
    pub taskbar_button_hover: Color,
    pub taskbar_button_active: Color,
    pub taskbar_button_border: Color,
    pub taskbar_separator: Color,
    pub taskbar_clock_text: Color,
    pub taskbar_branding_accent: Color,
    pub taskbar_branding_text: Color,

    // Widgets
    pub background: Color,
    pub button_normal: Color,
    pub button_hover: Color,
    pub button_pressed: Color,
    pub input_bg: Color,
    pub input_border: Color,
    pub text_primary: Color,
    pub text_placeholder: Color,
    pub focus_ring: Color,
    pub checkbox_check: Color,
    pub slider_track: Color,
    pub slider_thumb: Color,
    pub slider_thumb_hover: Color,
    pub label_text: Color,

    // Terminal
    pub terminal_bg: Color,
    pub terminal_fg: Color,
    pub terminal_cursor: Color,
    pub terminal_selection: Color,
}

impl Theme {
    /// The default dark blue-gray theme.
    pub const DEFAULT: Theme = Theme {
        // Desktop (Ayu Dark inspired -- warm dark tones)
        desktop_bg_top: Color::from_rgb(0x0F, 0x14, 0x19),
        desktop_bg_bottom: Color::from_rgb(0x0A, 0x0E, 0x12),

        // Window decorations
        title_active_top: Color::from_rgb(0x1A, 0x1F, 0x29),
        title_active_bottom: Color::from_rgb(0x13, 0x17, 0x1F),
        title_inactive_top: Color::from_rgb(0x16, 0x1A, 0x22),
        title_inactive_bottom: Color::from_rgb(0x10, 0x14, 0x1A),
        window_border_highlight: Color::from_rgb(0x2A, 0x30, 0x3C),
        window_border_shadow: Color::from_rgb(0x0B, 0x0E, 0x14),
        window_shadow: Color::from_rgb(0x05, 0x07, 0x0A),
        close_button_normal: Color::from_rgb(0x90, 0x3A, 0x3E), // Subdued red
        close_button_hover: Color::from_rgb(0xC0, 0x50, 0x55),
        close_button_x: Color::from_rgb(0xC0, 0xC0, 0xC0),

        // Taskbar
        taskbar_bg_top: Color::from_rgb(0x13, 0x17, 0x1F),
        taskbar_bg_bottom: Color::from_rgb(0x0D, 0x10, 0x17),
        taskbar_text: Color::from_rgb(0xB8, 0xC4, 0xD0),
        taskbar_button_normal: Color::from_rgb(0x1A, 0x1F, 0x29),
        taskbar_button_hover: Color::from_rgb(0x22, 0x28, 0x34),
        taskbar_button_active: Color::from_rgb(0x1E, 0x3A, 0x50), // Muted blue highlight
        taskbar_button_border: Color::from_rgb(0x27, 0x2D, 0x38),
        taskbar_separator: Color::from_rgb(0x27, 0x2D, 0x38),
        taskbar_clock_text: Color::from_rgb(0x6C, 0x73, 0x80),
        taskbar_branding_accent: Color::from_rgb(0xE6, 0xB4, 0x50), // Ayu orange/yellow
        taskbar_branding_text: Color::from_rgb(0xE6, 0xB4, 0x50),

        // Widgets (Ayu Dark)
        background: Color::from_rgb(0x0B, 0x0E, 0x14),
        button_normal: Color::from_rgb(0x1A, 0x1F, 0x29),
        button_hover: Color::from_rgb(0x22, 0x28, 0x34),
        button_pressed: Color::from_rgb(0x14, 0x18, 0x20),
        input_bg: Color::from_rgb(0x0D, 0x10, 0x17),
        input_border: Color::from_rgb(0x27, 0x2D, 0x38),
        text_primary: Color::from_rgb(0xCB, 0xCC, 0xC6), // Ayu fg
        text_placeholder: Color::from_rgb(0x5C, 0x63, 0x70),
        focus_ring: Color::from_rgb(0x39, 0xBA, 0xE6), // Ayu blue
        checkbox_check: Color::from_rgb(0x39, 0xBA, 0xE6),
        slider_track: Color::from_rgb(0x1A, 0x1F, 0x29),
        slider_thumb: Color::from_rgb(0x39, 0xBA, 0xE6),
        slider_thumb_hover: Color::from_rgb(0x59, 0xCA, 0xF6),
        label_text: Color::from_rgb(0xCB, 0xCC, 0xC6),

        // Terminal (Ayu Dark)
        terminal_bg: Color::from_rgb(0x0B, 0x0E, 0x14),
        terminal_fg: Color::from_rgb(0xCB, 0xCC, 0xC6),
        terminal_cursor: Color::from_rgb(0xE6, 0xB4, 0x50), // Ayu orange cursor
        terminal_selection: Color::from_rgb(0x27, 0x2D, 0x38),
    };
}

/// Linear interpolation between two colors. `t=0` gives `a`, `t=255` gives `b`.
pub const fn lerp_color(a: Color, b: Color, t: u8) -> Color {
    let t = t as u32;
    let inv = 255 - t;
    let r = (a.red() as u32 * inv + b.red() as u32 * t) / 255;
    let g = (a.green() as u32 * inv + b.green() as u32 * t) / 255;
    let b_ch = (a.blue() as u32 * inv + b.blue() as u32 * t) / 255;
    Color::from_rgb(r as u8, g as u8, b_ch as u8)
}

/// Fill a rectangle with a vertical gradient from `top_color` to `bottom_color`.
pub fn draw_gradient_v(
    buffer: &mut [u32],
    buffer_width: u32,
    buffer_height: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    top_color: Color,
    bottom_color: Color,
) {
    use crate::widgets::draw_rect;
    if h == 0 {
        return;
    }
    for row in 0..h {
        let t = ((row as u64 * 255) / (h as u64 - 1).max(1)) as u8;
        let color = lerp_color(top_color, bottom_color, t);
        draw_rect(
            buffer,
            buffer_width,
            buffer_height,
            x,
            y + row as i32,
            w,
            1,
            color.raw(),
        );
    }
}

/// Draw a vertical gradient directly to a `Screen` from `top_color` to `bottom_color`.
pub fn draw_gradient_v_screen(
    screen: &mut Screen,
    x: u64,
    y: u64,
    w: u64,
    h: u64,
    top_color: Color,
    bottom_color: Color,
) {
    if h == 0 {
        return;
    }
    for row in 0..h {
        let t = ((row * 255) / (h - 1).max(1)) as u8;
        let color = lerp_color(top_color, bottom_color, t);
        let _ = screen.draw_rect(x, y + row, w, 1, color);
    }
}
