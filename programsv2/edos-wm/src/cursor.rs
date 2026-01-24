//! Cursor rendering for the window manager.

use edos_render::graphics::{Color, Texture};

/// Standard cursor size.
pub const CURSOR_SIZE: u64 = 16;

/// Arrow cursor bitmap.
/// 0 = transparent, 1 = white (fill), 2 = black (outline)
#[rustfmt::skip]
const ARROW: [[u8; 16]; 16] = [
    [2,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,2,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,2,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,2,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,2,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,2,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,2,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,1,2,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,1,1,2,0,0,0,0,0,0,0],
    [2,1,1,1,1,1,1,1,1,2,0,0,0,0,0,0],
    [2,1,1,1,1,1,2,2,2,2,2,0,0,0,0,0],
    [2,1,1,2,1,1,2,0,0,0,0,0,0,0,0,0],
    [2,1,2,0,2,1,1,2,0,0,0,0,0,0,0,0],
    [2,2,0,0,2,1,1,2,0,0,0,0,0,0,0,0],
    [2,0,0,0,0,2,1,1,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,2,2,2,2,0,0,0,0,0,0,0],
];

/// Load the default arrow cursor as a texture.
pub fn load_default() -> Texture {
    let mut texture = Texture::new(CURSOR_SIZE, CURSOR_SIZE).unwrap();

    for y in 0..16 {
        for x in 0..16 {
            let color = match ARROW[y][x] {
                1 => Color::WHITE,
                2 => Color::BLACK,
                _ => continue, // Transparent - don't set pixel
            };
            let _ = texture.set_pixel(x as u64, y as u64, color);
        }
    }

    texture
}

/// Cursor state tracking.
pub struct Cursor {
    pub texture: Texture,
    pub x: i32,
    pub y: i32,
}

impl Cursor {
    /// Create a new cursor with the default arrow texture.
    pub fn new() -> Self {
        Self {
            texture: load_default(),
            x: 0,
            y: 0,
        }
    }

    /// Update cursor position.
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}
