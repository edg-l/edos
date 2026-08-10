//! Reading an image off disk and fitting it to a rectangle.
//!
//! BMP because it is the one raster format a machine can produce without a
//! library and this OS can decode without one: no compression to implement, no
//! entropy coder, no colour management. Anything richer belongs behind a real
//! decoder crate, and none of them build against this std yet.

use crate::graphics::Color;

/// A decoded image, one packed `0xAARRGGBB` word per pixel, top row first.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ImageError {
    /// Not a BMP, or truncated before the pixel data.
    Malformed,
    /// A BMP this decoder does not read: compressed, paletted, or 1/4/8/16 bpp.
    Unsupported,
}

/// Offset of the pixel-data pointer in `BITMAPFILEHEADER`.
const PIXEL_OFFSET: usize = 10;

/// Size of `BITMAPFILEHEADER`, and so the offset of the DIB header.
const DIB_START: usize = 14;

/// Smallest DIB header that carries width, height and bit depth.
const BITMAPINFOHEADER: u32 = 40;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Decode an uncompressed 24- or 32-bit BMP.
///
/// Later DIB headers (V4, V5) are read as `BITMAPINFOHEADER`: everything they
/// add describes colour management and masks that a plain BGR(A) image does not
/// use, and the fields this decoder reads sit at the same offsets in all of
/// them.
pub fn decode_bmp(bytes: &[u8]) -> Result<Image, ImageError> {
    if bytes.len() < DIB_START + BITMAPINFOHEADER as usize || &bytes[..2] != b"BM" {
        return Err(ImageError::Malformed);
    }
    let dib_size = u32_at(bytes, DIB_START);
    if dib_size < BITMAPINFOHEADER {
        return Err(ImageError::Unsupported);
    }

    let width = u32_at(bytes, DIB_START + 4) as i32;
    // A negative height means the rows are stored top-down.
    let raw_height = u32_at(bytes, DIB_START + 8) as i32;
    let bpp = u16_at(bytes, DIB_START + 14);
    let compression = u32_at(bytes, DIB_START + 16);

    // BI_BITFIELDS is accepted at 32 bpp only, where the masks a writer can
    // choose are in practice the BGRA layout BI_RGB already means.
    if !(compression == 0 || (compression == 3 && bpp == 32)) {
        return Err(ImageError::Unsupported);
    }
    if bpp != 24 && bpp != 32 {
        return Err(ImageError::Unsupported);
    }
    if width <= 0 || raw_height == 0 {
        return Err(ImageError::Malformed);
    }

    let top_down = raw_height < 0;
    let height = raw_height.unsigned_abs();
    let width = width as u32;

    let bytes_per_pixel = bpp as usize / 8;
    // Rows are padded to a four-byte boundary.
    let stride = (width as usize * bytes_per_pixel).div_ceil(4) * 4;
    let start = u32_at(bytes, PIXEL_OFFSET) as usize;
    let needed = start
        .checked_add(stride * height as usize)
        .ok_or(ImageError::Malformed)?;
    if needed > bytes.len() {
        return Err(ImageError::Malformed);
    }

    let mut pixels = vec![0u32; width as usize * height as usize];
    for row in 0..height as usize {
        let src_row = if top_down {
            row
        } else {
            height as usize - 1 - row
        };
        let src = &bytes[start + src_row * stride..];
        let dst = &mut pixels[row * width as usize..(row + 1) * width as usize];
        for (x, px) in dst.iter_mut().enumerate() {
            let at = x * bytes_per_pixel;
            // BMP stores BGR, and the fourth byte is alpha this decoder ignores:
            // a wallpaper composites against nothing.
            *px = Color::from_rgb(src[at + 2], src[at + 1], src[at]).raw();
        }
    }

    Ok(Image {
        width,
        height,
        pixels,
    })
}

/// 16.16 fixed point throughout: the source step is a fraction and a float
/// would be the only float in the compositor.
const FRAC: u64 = 1 << 16;

impl Image {
    /// Scale to cover `width` x `height`, preserving aspect ratio and cropping
    /// the overflowing axis about the centre.
    ///
    /// Cover rather than fit: a letterboxed wallpaper puts two bars of dead
    /// colour on a desktop whose whole job is to be a ground, and the edges of
    /// a background are where the least happens anyway.
    pub fn scaled_to_cover(&self, width: u32, height: u32) -> Vec<u32> {
        if width == 0 || height == 0 || self.width == 0 || self.height == 0 {
            return vec![0u32; width as usize * height as usize];
        }
        let scale_x = (self.width as u64 * FRAC) / width as u64;
        let scale_y = (self.height as u64 * FRAC) / height as u64;
        // The smaller step is the one that covers: taking it on both axes keeps
        // the aspect ratio and overflows the other axis, which is then cropped.
        let step = scale_x.min(scale_y).max(1);
        let origin_x = (self.width as u64 * FRAC).saturating_sub(step * width as u64) / 2;
        let origin_y = (self.height as u64 * FRAC).saturating_sub(step * height as u64) / 2;
        self.resample(width, height, step, origin_x, origin_y)
    }

    /// The largest size that fits inside `width` x `height` with the aspect
    /// ratio kept. Never enlarges: an image smaller than the box keeps its own
    /// size, since a viewer that magnifies by default hides what it was handed.
    pub fn fit_size(&self, width: u32, height: u32) -> (u32, u32) {
        if self.width == 0 || self.height == 0 {
            return (0, 0);
        }
        if self.width <= width && self.height <= height {
            return (self.width, self.height);
        }
        let by_width = (self.height as u64 * width as u64 / self.width as u64).max(1);
        if by_width <= height as u64 {
            (width.max(1), by_width as u32)
        } else {
            let by_height = (self.width as u64 * height as u64 / self.height as u64).max(1);
            (by_height as u32, height.max(1))
        }
    }

    /// Scale the whole image into `width` x `height`, which is expected to come
    /// from [`Image::fit_size`]: nothing is cropped, so a box of another aspect
    /// ratio stretches rather than letterboxing. The caller owns the letterbox,
    /// because only it knows what colour the surrounding surface is.
    pub fn scaled_to_fit(&self, width: u32, height: u32) -> Vec<u32> {
        if width == 0 || height == 0 || self.width == 0 || self.height == 0 {
            return vec![0u32; width as usize * height as usize];
        }
        let scale_x = ((self.width as u64 * FRAC) / width as u64).max(1);
        let scale_y = ((self.height as u64 * FRAC) / height as u64).max(1);
        self.resample_axes(width, height, scale_x, scale_y)
    }

    /// Bilinear resample with one step on both axes, starting `origin` into the
    /// source.
    fn resample(
        &self,
        width: u32,
        height: u32,
        step: u64,
        origin_x: u64,
        origin_y: u64,
    ) -> Vec<u32> {
        self.resample_at(width, height, step, step, origin_x, origin_y)
    }

    /// Bilinear resample with a per-axis step, from the source origin.
    fn resample_axes(&self, width: u32, height: u32, step_x: u64, step_y: u64) -> Vec<u32> {
        self.resample_at(width, height, step_x, step_y, 0, 0)
    }

    fn resample_at(
        &self,
        width: u32,
        height: u32,
        step_x: u64,
        step_y: u64,
        origin_x: u64,
        origin_y: u64,
    ) -> Vec<u32> {
        let mut out = vec![0u32; width as usize * height as usize];
        for y in 0..height as u64 {
            let sy = origin_y + y * step_y;
            let row = (sy >> 16).min(self.height as u64 - 1);
            let next_row = (row + 1).min(self.height as u64 - 1);
            let wy = ((sy & 0xFFFF) >> 8) as u32;
            for x in 0..width as u64 {
                let sx = origin_x + x * step_x;
                let col = (sx >> 16).min(self.width as u64 - 1);
                let next_col = (col + 1).min(self.width as u64 - 1);
                let wx = ((sx & 0xFFFF) >> 8) as u32;

                let at = |r: u64, c: u64| self.pixels[(r * self.width as u64 + c) as usize];
                let top = lerp8(at(row, col), at(row, next_col), wx);
                let bottom = lerp8(at(next_row, col), at(next_row, next_col), wx);
                out[(y * width as u64 + x) as usize] = lerp8(top, bottom, wy);
            }
        }
        out
    }
}

/// Blend two packed colours, `t` running 0..=255 from `a` to `b`.
fn lerp8(a: u32, b: u32, t: u32) -> u32 {
    let channel = |shift: u32| {
        let a = (a >> shift) & 0xFF;
        let b = (b >> shift) & 0xFF;
        ((a * (255 - t) + b * t) / 255) & 0xFF
    };
    0xFF00_0000 | (channel(16) << 16) | (channel(8) << 8) | channel(0)
}
