//! Cursor rendering for the window manager.

use edos_render::graphics::{Color, Texture};

/// Standard cursor size.
pub const CURSOR_SIZE: u64 = 16;

/// Cursor shape variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorShape {
    Arrow,
    ResizeH,     // left-right
    ResizeV,     // up-down
    ResizeFDiag, // top-left to bottom-right (NW-SE)
    ResizeBDiag, // top-right to bottom-left (NE-SW)
}

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

/// Horizontal double-arrow cursor (left-right resize).
#[rustfmt::skip]
const RESIZE_H: [[u8; 16]; 16] = [
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,2,0,0,0,0,0,0,0,2,0,0,0,0],
    [0,0,2,2,0,0,0,0,0,0,0,2,2,0,0,0],
    [0,2,1,2,2,2,2,2,2,2,2,2,1,2,0,0],
    [2,1,1,1,1,1,1,1,1,1,1,1,1,1,2,0],
    [2,1,1,1,1,1,1,1,1,1,1,1,1,1,2,0],
    [0,2,1,2,2,2,2,2,2,2,2,2,1,2,0,0],
    [0,0,2,2,0,0,0,0,0,0,0,2,2,0,0,0],
    [0,0,0,2,0,0,0,0,0,0,0,2,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
];

/// Vertical double-arrow cursor (up-down resize).
#[rustfmt::skip]
const RESIZE_V: [[u8; 16]; 16] = [
    [0,0,0,0,0,0,0,2,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,2,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,2,1,1,1,2,0,0,0,0,0,0],
    [0,0,0,0,0,2,2,1,2,2,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,1,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,1,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,1,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,1,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,1,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,1,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,2,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,2,1,1,1,2,0,0,0,0,0,0],
    [0,0,0,0,0,2,2,1,2,2,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,2,2,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,2,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
];

/// Forward diagonal double-arrow cursor (NW-SE resize).
#[rustfmt::skip]
const RESIZE_FDIAG: [[u8; 16]; 16] = [
    [2,2,2,2,2,2,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,1,2,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,1,2,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,1,2,0,0,0,0,0,0,0,0,0,0,0,0],
    [2,1,2,0,2,0,0,0,0,0,0,0,0,0,0,0],
    [2,2,0,0,0,2,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,2,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,2,0,0,0,2,2,0,0],
    [0,0,0,0,0,0,0,0,0,2,0,2,1,2,0,0],
    [0,0,0,0,0,0,0,0,0,0,2,1,1,2,0,0],
    [0,0,0,0,0,0,0,0,0,2,1,1,1,2,0,0],
    [0,0,0,0,0,0,0,0,2,1,1,1,1,2,0,0],
    [0,0,0,0,0,0,0,0,2,2,2,2,2,2,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
];

/// Backward diagonal double-arrow cursor (NE-SW resize).
#[rustfmt::skip]
const RESIZE_BDIAG: [[u8; 16]; 16] = [
    [0,0,0,0,0,0,0,0,2,2,2,2,2,2,0,0],
    [0,0,0,0,0,0,0,0,2,1,1,1,1,2,0,0],
    [0,0,0,0,0,0,0,0,0,2,1,1,1,2,0,0],
    [0,0,0,0,0,0,0,0,0,0,2,1,1,2,0,0],
    [0,0,0,0,0,0,0,0,0,2,0,2,1,2,0,0],
    [0,0,0,0,0,0,0,0,2,0,0,0,2,2,0,0],
    [0,0,0,0,0,0,0,2,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,2,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,2,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,2,0,0,0,2,0,0,0,0,0,0,0],
    [0,0,0,2,1,2,0,0,0,0,0,0,0,0,0,0],
    [0,0,2,1,1,1,2,0,0,0,0,0,0,0,0,0],
    [0,2,1,1,1,1,1,2,0,0,0,0,0,0,0,0],
    [2,2,2,2,2,2,2,2,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
    [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
];

/// Build a cursor texture from a 16x16 bitmap.
/// Encoding: 0 = transparent, 1 = white (fill), 2 = black (outline).
fn build_cursor_texture(bitmap: &[[u8; 16]; 16]) -> Texture {
    let mut texture = Texture::new(CURSOR_SIZE, CURSOR_SIZE).unwrap();

    for (y, row) in bitmap.iter().enumerate() {
        for (x, cell) in row.iter().enumerate() {
            let color = match cell {
                1 => Color::WHITE,
                2 => Color::BLACK,
                _ => continue, // transparent - skip
            };
            let _ = texture.set_pixel(x as u64, y as u64, color);
        }
    }

    texture
}

/// Cursor state tracking.
pub struct Cursor {
    pub x: i32,
    pub y: i32,
    textures: [Texture; 5], // indexed by CursorShape as usize
    current_shape: CursorShape,
}

impl CursorShape {
    fn index(self) -> usize {
        match self {
            CursorShape::Arrow => 0,
            CursorShape::ResizeH => 1,
            CursorShape::ResizeV => 2,
            CursorShape::ResizeFDiag => 3,
            CursorShape::ResizeBDiag => 4,
        }
    }
}

impl Cursor {
    /// Create a new cursor with all shape textures pre-built.
    pub fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            textures: [
                build_cursor_texture(&ARROW),
                build_cursor_texture(&RESIZE_H),
                build_cursor_texture(&RESIZE_V),
                build_cursor_texture(&RESIZE_FDIAG),
                build_cursor_texture(&RESIZE_BDIAG),
            ],
            current_shape: CursorShape::Arrow,
        }
    }

    /// Update cursor position.
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    /// Change the active cursor shape.
    pub fn set_shape(&mut self, shape: CursorShape) {
        self.current_shape = shape;
    }

    /// The active shape, so a caller holding a copy of the cursor image can
    /// tell when it has gone stale.
    pub fn shape(&self) -> CursorShape {
        self.current_shape
    }

    /// Get the texture for the current cursor shape.
    pub fn current_texture(&self) -> &Texture {
        &self.textures[self.current_shape.index()]
    }
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}
