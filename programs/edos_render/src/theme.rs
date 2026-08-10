//! Theme system providing color constants and drawing helpers for the EDOS GUI.

use crate::graphics::{Color, Screen};

/// Central theme struct holding all color constants for the EDOS GUI.
pub struct Theme {
    // Desktop
    pub desktop_bg_top: Color,
    pub desktop_bg_bottom: Color,
    /// Brightest point of the desktop, under the light centre.
    pub desktop_glow: Color,
    /// Darkest point of the desktop, at the corners furthest from the light.
    pub desktop_edge: Color,

    // Window decorations
    pub title_active_top: Color,
    pub title_active_bottom: Color,
    pub title_inactive_top: Color,
    pub title_inactive_bottom: Color,
    pub window_border_highlight: Color,
    pub window_border_shadow: Color,
    pub window_shadow: Color,
    /// Hairline along the top of the focused window's title bar.
    pub title_accent: Color,
    pub title_text: Color,
    pub title_text_inactive: Color,
    pub close_button_hover: Color,
    pub close_glyph: Color,
    pub close_glyph_hover: Color,

    // Taskbar
    pub taskbar_bg_top: Color,
    pub taskbar_bg_bottom: Color,
    pub taskbar_text: Color,
    /// Label of the focused window's button, and of the launcher.
    pub taskbar_text_active: Color,
    pub taskbar_button_normal: Color,
    pub taskbar_button_active: Color,
    /// Underline marking the focused window's button.
    pub taskbar_button_accent: Color,
    pub taskbar_button_border: Color,
    pub taskbar_separator: Color,
    pub taskbar_clock_text: Color,
    pub taskbar_branding_text: Color,

    // Widgets
    pub background: Color,
    pub button_normal: Color,
    pub button_hover: Color,
    pub button_pressed: Color,
    pub input_bg: Color,
    pub input_border: Color,
    /// Border of a hovered or pressed control. Distinct from `focus_ring`, so
    /// the pointer being over a control never looks like keyboard focus.
    pub border_hover: Color,
    pub text_primary: Color,
    pub text_placeholder: Color,
    pub focus_ring: Color,
    /// Fill of a control that is present but cannot be used.
    pub control_disabled: Color,
    /// Label of a control that is present but cannot be used.
    pub text_disabled: Color,
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
        desktop_bg_top: Color::from_rgb(0x11, 0x16, 0x1D),
        desktop_bg_bottom: Color::from_rgb(0x0B, 0x0F, 0x15),
        desktop_glow: Color::from_rgb(0x1B, 0x1D, 0x22),
        desktop_edge: Color::from_rgb(0x06, 0x08, 0x0D),

        // Window decorations
        title_active_top: Color::from_rgb(0x1F, 0x24, 0x30),
        title_active_bottom: Color::from_rgb(0x16, 0x1B, 0x24),
        title_inactive_top: Color::from_rgb(0x11, 0x15, 0x1C),
        title_inactive_bottom: Color::from_rgb(0x0E, 0x12, 0x18),
        window_border_highlight: Color::from_rgb(0x2A, 0x30, 0x3C),
        window_border_shadow: Color::from_rgb(0x0B, 0x0E, 0x14),
        window_shadow: Color::from_rgb(0x05, 0x07, 0x0A),
        title_accent: Color::from_rgb(0xE6, 0xB4, 0x50), // Ayu orange, the one accent
        title_text: Color::from_rgb(0xCB, 0xCC, 0xC6),
        title_text_inactive: Color::from_rgb(0x6C, 0x73, 0x80),
        close_button_hover: Color::from_rgb(0xC0, 0x50, 0x55),
        close_glyph: Color::from_rgb(0x8A, 0x91, 0x99),
        close_glyph_hover: Color::from_rgb(0xF2, 0xF2, 0xF0),

        // Taskbar
        taskbar_bg_top: Color::from_rgb(0x13, 0x17, 0x1F),
        taskbar_bg_bottom: Color::from_rgb(0x0D, 0x10, 0x17),
        taskbar_text: Color::from_rgb(0x8A, 0x91, 0x99),
        taskbar_text_active: Color::from_rgb(0xCB, 0xCC, 0xC6),
        taskbar_button_normal: Color::from_rgb(0x1A, 0x1F, 0x29),
        taskbar_button_active: Color::from_rgb(0x23, 0x29, 0x35),
        taskbar_button_accent: Color::from_rgb(0xE6, 0xB4, 0x50), // Ayu orange, the one accent
        taskbar_button_border: Color::from_rgb(0x27, 0x2D, 0x38),
        taskbar_separator: Color::from_rgb(0x27, 0x2D, 0x38),
        taskbar_clock_text: Color::from_rgb(0x6C, 0x73, 0x80),
        taskbar_branding_text: Color::from_rgb(0x8A, 0x91, 0x99),

        // Widgets (Ayu Dark)
        background: Color::from_rgb(0x0B, 0x0E, 0x14),
        button_normal: Color::from_rgb(0x1A, 0x1F, 0x29),
        button_hover: Color::from_rgb(0x22, 0x28, 0x34),
        button_pressed: Color::from_rgb(0x14, 0x18, 0x20),
        input_bg: Color::from_rgb(0x0D, 0x10, 0x17),
        input_border: Color::from_rgb(0x27, 0x2D, 0x38),
        border_hover: Color::from_rgb(0x3E, 0x46, 0x54),
        text_primary: Color::from_rgb(0xCB, 0xCC, 0xC6), // Ayu fg
        text_placeholder: Color::from_rgb(0x5C, 0x63, 0x70),
        focus_ring: Color::from_rgb(0xE6, 0xB4, 0x50), // Ayu orange, the one accent
        // A disabled control stays legible and stops inviting the pointer: the
        // fill recedes toward the panel and the label loses contrast, so it
        // reads as present-but-inert rather than as missing.
        control_disabled: Color::from_rgb(0x16, 0x1B, 0x22),
        text_disabled: Color::from_rgb(0x4B, 0x53, 0x5E),
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
