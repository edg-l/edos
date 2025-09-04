#![expect(unused)]

pub const BLACK: u32 = 0x000000FF;
pub const WHITE: u32 = 0xFFFFFFFF;
pub const RED: u32 = 0xFF0000FF;
pub const GREEN: u32 = 0x00FF00FF;
pub const BLUE: u32 = 0x0000FFFF;
pub const YELLOW: u32 = 0xFFFF00FF;
pub const MAGENTA: u32 = 0xFF00FFFF;
pub const CYAN: u32 = 0x00FFFFFF;

// Grays
pub const GRAY: u32 = 0x808080FF;
pub const LIGHT_GRAY: u32 = 0xC0C0C0FF;
pub const DARK_GRAY: u32 = 0x404040FF;

// Common UI colors
pub const ORANGE: u32 = 0xFF8000FF;
pub const PURPLE: u32 = 0x8000FFFF;
pub const BROWN: u32 = 0x964B00FF;
pub const PINK: u32 = 0xFF69B4FF;

// Terminal colors
pub const TERM_BLACK: u32 = 0x000000FF;
pub const TERM_RED: u32 = 0xCD0000FF;
pub const TERM_GREEN: u32 = 0x00CD00FF;
pub const TERM_YELLOW: u32 = 0xCDCD00FF;
pub const TERM_BLUE: u32 = 0x0000EEFF;
pub const TERM_MAGENTA: u32 = 0xCD00CDFF;
pub const TERM_CYAN: u32 = 0x00CDCDFF;
pub const TERM_WHITE: u32 = 0xE5E5E5FF;

// Transparent
pub const TRANSPARENT: u32 = 0x00000000;

// Helper function to create custom colors
#[inline]
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    ((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | (a as u32)
}

// Helper function for opaque colors
#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    rgba(r, g, b, 255)
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

    rgba(r, g, b, 255)
}
