#![expect(unused)]

pub const BLACK: u32 = 0x00000000;
pub const WHITE: u32 = 0x00FFFFFF;
pub const RED: u32 = 0x00FF0000;
pub const GREEN: u32 = 0x0000FF00;
pub const BLUE: u32 = 0x000000FF;
pub const YELLOW: u32 = 0x00FFFF00;
pub const MAGENTA: u32 = 0x00FF00FF;
pub const CYAN: u32 = 0x0000FFFF;

// Grays
pub const GRAY: u32 = 0x00808080;
pub const LIGHT_GRAY: u32 = 0x00C0C0C0;
pub const DARK_GRAY: u32 = 0x00404040;

// Common UI colors
pub const ORANGE: u32 = 0x00FF8000;
pub const PURPLE: u32 = 0x008000FF;
pub const BROWN: u32 = 0x00964B00;
pub const PINK: u32 = 0x00FF69B4;

// Terminal colors
pub const TERM_BLACK: u32 = 0x00000000;
pub const TERM_RED: u32 = 0x00CD0000;
pub const TERM_GREEN: u32 = 0x0000CD00;
pub const TERM_YELLOW: u32 = 0x00CDCD00;
pub const TERM_BLUE: u32 = 0x000000EE;
pub const TERM_MAGENTA: u32 = 0x00CD00CD;
pub const TERM_CYAN: u32 = 0x0000CDCD;
pub const TERM_WHITE: u32 = 0x00E5E5E5;

// Helper function to create RGB colors
#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

// Helper function for rainbow colors
pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> u32 {
    let h = h / 60.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = l - c / 2.0;

    let (r, g, b) = if h < 1.0 {
        (c, x, 0.0)
    } else if h < 2.0 {
        (x, c, 0.0)
    } else if h < 3.0 {
        (0.0, c, x)
    } else if h < 4.0 {
        (0.0, x, c)
    } else if h < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    let r = ((r + m) * 255.0) as u8;
    let g = ((g + m) * 255.0) as u8;
    let b = ((b + m) * 255.0) as u8;

    rgb(r, g, b)
}
