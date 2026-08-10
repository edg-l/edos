//! Monochrome icons for the shell.
//!
//! Drawn as 16x16 coverage masks and tinted at draw time, so an icon takes the
//! colour of whatever state it is in: muted at rest, bright when the control is
//! active, disabled with the label. That is why they are masks and not images.
//!
//! Hand-authored rather than loaded from a theme: a desktop with eight icons
//! does not need an icon theme, a lookup path or a cache, and every one of
//! those is a thing that can be missing at boot.

/// Edge length of every icon, in pixels.
pub const SIZE: usize = 16;

/// A 16x16 coverage mask. `1` is ink, `0` is nothing; drawn at whatever colour
/// the caller passes.
pub type Mask = [u16; SIZE];

/// Terminal: a window with a prompt caret.
pub const TERMINAL: Mask = [
    0b0000000000000000,
    0b0111111111111110,
    0b0100000000000010,
    0b0111111111111110,
    0b0100000000000010,
    0b0100000000000010,
    0b0100110000000010,
    0b0100011000000010,
    0b0100001100000010,
    0b0100011000000010,
    0b0100110000000010,
    0b0100000000000010,
    0b0100001111100010,
    0b0100000000000010,
    0b0111111111111110,
    0b0000000000000000,
];

/// Application grid: the launcher.
pub const APPS: Mask = [
    0b0000000000000000,
    0b0000000000000000,
    0b0011000110001100,
    0b0011000110001100,
    0b0000000000000000,
    0b0000000000000000,
    0b0011000110001100,
    0b0011000110001100,
    0b0000000000000000,
    0b0000000000000000,
    0b0011000110001100,
    0b0011000110001100,
    0b0000000000000000,
    0b0000000000000000,
    0b0000000000000000,
    0b0000000000000000,
];

/// Speaker with waves: volume.
pub const VOLUME: Mask = [
    0b0000000000000000,
    0b0000000110000000,
    0b0000001110000000,
    0b0000011110001000,
    0b0001111110010100,
    0b0001111110100100,
    0b0001111110100100,
    0b0001111110100100,
    0b0001111110100100,
    0b0001111110100100,
    0b0000011110010100,
    0b0000001110001000,
    0b0000000110000000,
    0b0000000000000000,
    0b0000000000000000,
    0b0000000000000000,
];

/// Two arrows, up and down: the network link.
pub const NETWORK: Mask = [
    0b0000000000000000,
    0b0000110000000000,
    0b0001110000110000,
    0b0011110000110000,
    0b0000110000110000,
    0b0000110000110000,
    0b0000110000110000,
    0b0000110000110000,
    0b0000110000110000,
    0b0000110000110000,
    0b0000110000110000,
    0b0000110000111100,
    0b0000110000111000,
    0b0000110000110000,
    0b0000000000000000,
    0b0000000000000000,
];

/// Clock face.
pub const CLOCK: Mask = [
    0b0000001111000000,
    0b0000110000110000,
    0b0001000000001000,
    0b0010000110000100,
    0b0100000110000010,
    0b0100000110000010,
    0b1000000110000001,
    0b1000000110000001,
    0b1000000111111001,
    0b1000000000000001,
    0b0100000000000010,
    0b0100000000000010,
    0b0010000000000100,
    0b0001000000001000,
    0b0000110000110000,
    0b0000001111000000,
];

/// Power: the ring and stroke, for shutting the machine down.
pub const POWER: Mask = [
    0b0000000110000000,
    0b0000000110000000,
    0b0001100110011000,
    0b0011000110001100,
    0b0110000110000110,
    0b0100000110000010,
    0b1100000110000011,
    0b1100000000000011,
    0b1100000000000011,
    0b1100000000000011,
    0b0100000000000010,
    0b0110000000000110,
    0b0011000000001100,
    0b0001110000111000,
    0b0000011111100000,
    0b0000000000000000,
];

/// Downward chevron: minimize a window.
pub const MINIMIZE: Mask = [
    0b0000000000000000,
    0b0000000000000000,
    0b0000000000000000,
    0b0000000000000000,
    0b0000000000000000,
    0b1100000000000110,
    0b1110000000001110,
    0b0111000000011100,
    0b0011100000111000,
    0b0001110001110000,
    0b0000111011100000,
    0b0000011111000000,
    0b0000001110000000,
    0b0000000000000000,
    0b0000000000000000,
    0b0000000000000000,
];

/// Empty square: maximize a window.
pub const MAXIMIZE: Mask = [
    0b0000000000000000,
    0b0000000000000000,
    0b0111111111111110,
    0b0111111111111110,
    0b0110000000000110,
    0b0110000000000110,
    0b0110000000000110,
    0b0110000000000110,
    0b0110000000000110,
    0b0110000000000110,
    0b0110000000000110,
    0b0110000000000110,
    0b0111111111111110,
    0b0000000000000000,
    0b0000000000000000,
    0b0000000000000000,
];

/// Draw an icon into a pixel buffer with its top-left corner at (`x`, `y`).
pub fn draw(
    buffer: &mut [u32],
    buffer_width: u32,
    buffer_height: u32,
    x: i32,
    y: i32,
    mask: &Mask,
    color: u32,
) {
    for (row, bits) in mask.iter().enumerate() {
        let py = y + row as i32;
        if py < 0 || py >= buffer_height as i32 {
            continue;
        }
        for col in 0..SIZE {
            // Bit 15 is the leftmost pixel, so the literal in the source reads
            // the same way round as the icon looks.
            if bits & (1 << (SIZE - 1 - col)) == 0 {
                continue;
            }
            let px = x + col as i32;
            if px < 0 || px >= buffer_width as i32 {
                continue;
            }
            let idx = (py as u32 * buffer_width + px as u32) as usize;
            if let Some(dst) = buffer.get_mut(idx) {
                *dst = color;
            }
        }
    }
}
