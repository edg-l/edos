use std::{fs::File, os::edos::io::FileExt};

// Re-export noto-sans-mono-bitmap types for user programs
pub use noto_sans_mono_bitmap::{FontWeight, RasterHeight};
use noto_sans_mono_bitmap::{get_raster, get_raster_width};
use thiserror::Error;

/// Graphics operation error type
#[derive(Debug, Error, Clone, Copy)]
pub enum GraphicsError {
    #[error("Invalid argument or coordinates")]
    InvalidInput,
    #[error("Out of memory")]
    OutOfMemory,
    #[error("Bad address/fault")]
    Fault,
    #[error("Unknown graphics error")]
    Unknown,
    #[error("Invalid color value")]
    InvalidColor,
    #[error("Coordinates out of bounds")]
    OutOfBounds,
    #[error("Unsupported character")]
    UnsupportedCharacter,
    #[error("Text rendering error")]
    TextError,
}

// Ioctl buffer flags
pub const IOCTL_ARG_IN: u64 = 1;
pub const IOCTL_ARG_OUT: u64 = 1 << 1;

const FB_IOCTL_DRAW_RECT: u64 = 0x4642_0001;
const FB_IOCTL_RENDER: u64 = 0x4642_0002;
const FB_IOCTL_DRAW: u64 = 0x4642_0003;
const FB_IOCTL_SCREEN_INFO: u64 = 0x4642_0004;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FramebufferRect {
    x: u64,
    y: u64,
    width: u64,
    height: u64,
    color: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FramebufferDraw {
    x: u64,
    y: u64,
    width: u64,
    height: u64,
    pixel_count: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct FramebufferInfo {
    width: u32,
    height: u32,
}

#[derive(Debug)]
pub struct Framebuffer {
    fd: File,
    buffer: Vec<u8>,
}

impl Framebuffer {
    pub fn new() -> Self {
        let file = File::open("/dev/fb").unwrap();
        Self {
            fd: file,
            buffer: Vec::with_capacity(size_of::<FramebufferDraw>()),
        }
    }

    /// Draw a rectangle directly to the screen
    pub fn draw_rect(&self, x: u64, y: u64, width: u64, height: u64, color: Color) {
        if width == 0 || height == 0 {
            return;
        }

        let mut rect = FramebufferRect {
            x,
            y,
            width,
            height,
            color: color.raw(),
            _padding: 0,
        };

        self.fd
            .ioctl(
                FB_IOCTL_DRAW_RECT,
                (&mut rect as *mut FramebufferRect) as u64,
                core::mem::size_of::<FramebufferRect>(),
                IOCTL_ARG_IN,
            )
            .unwrap();
    }

    /// Render all pending draw operations to the screen
    pub fn render(&self) {
        self.fd.ioctl(FB_IOCTL_RENDER, 0, 0, 0).unwrap();
    }

    /// Get screen information
    pub fn screen_info(&self) -> Result<ScreenInfo> {
        let mut info = FramebufferInfo::default();
        self.fd
            .ioctl(
                FB_IOCTL_SCREEN_INFO,
                (&mut info as *mut FramebufferInfo) as u64,
                core::mem::size_of::<FramebufferInfo>(),
                IOCTL_ARG_OUT,
            )
            .unwrap();

        Ok(ScreenInfo {
            width: info.width as usize,
            height: info.height as usize,
        })
    }

    /// Draw this request to the screen
    pub fn draw(&mut self, request: &DrawRequest) -> Result<()> {
        let pixel_count = request
            .width
            .checked_mul(request.height)
            .ok_or(GraphicsError::InvalidInput)?;

        if pixel_count == 0 {
            return Ok(());
        }

        let header = FramebufferDraw {
            x: request.x,
            y: request.y,
            width: request.width,
            height: request.height,
            pixel_count,
        };

        let header_bytes = core::mem::size_of::<FramebufferDraw>();
        let pixel_bytes = (pixel_count as usize)
            .checked_mul(core::mem::size_of::<u32>())
            .ok_or(GraphicsError::InvalidInput)?;

        self.buffer.clear();
        self.buffer.reserve(header_bytes + pixel_bytes);

        self.buffer.extend_from_slice(unsafe {
            core::slice::from_raw_parts(
                (&header as *const FramebufferDraw) as *const u8,
                header_bytes,
            )
        });
        self.buffer.extend_from_slice(unsafe {
            core::slice::from_raw_parts(request.pixels.as_ptr() as *const u8, pixel_bytes)
        });

        self.fd
            .ioctl(
                FB_IOCTL_DRAW,
                self.buffer.as_mut_ptr() as u64,
                self.buffer.len(),
                IOCTL_ARG_IN,
            )
            .unwrap();

        Ok(())
    }
}

pub type Result<T> = std::result::Result<T, GraphicsError>;

/// Type-safe color representation (RGB format)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(u32);

impl Color {
    /// Create a new color from RGB components
    #[inline]
    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self(((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }

    /// Get the raw u32 value
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Extract red component
    #[inline]
    pub const fn red(self) -> u8 {
        ((self.0 >> 16) & 0xFF) as u8
    }

    /// Extract green component
    #[inline]
    pub const fn green(self) -> u8 {
        ((self.0 >> 8) & 0xFF) as u8
    }

    /// Extract blue component
    #[inline]
    pub const fn blue(self) -> u8 {
        (self.0 & 0xFF) as u8
    }

    // Common color constants
    pub const BLACK: Color = Color::from_rgb(0, 0, 0);
    pub const WHITE: Color = Color::from_rgb(255, 255, 255);
    pub const RED: Color = Color::from_rgb(255, 0, 0);
    pub const GREEN: Color = Color::from_rgb(0, 255, 0);
    pub const BLUE: Color = Color::from_rgb(0, 0, 255);
    pub const YELLOW: Color = Color::from_rgb(255, 255, 0);
    pub const CYAN: Color = Color::from_rgb(0, 255, 255);
    pub const MAGENTA: Color = Color::from_rgb(255, 0, 255);
}

impl From<u32> for Color {
    #[inline]
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<Color> for u32 {
    #[inline]
    fn from(color: Color) -> Self {
        color.0
    }
}

/// Rectangle structure for specifying regions
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
}

impl Rect {
    #[inline]
    pub const fn new(x: u64, y: u64, width: u64, height: u64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Text styling configuration
#[derive(Debug, Clone, Copy)]
pub struct TextStyle {
    pub font_weight: FontWeight,
    pub font_size: RasterHeight,
    pub foreground: Color,
    pub background: Option<Color>,
}

impl TextStyle {
    /// Create a new text style with default settings
    #[inline]
    pub const fn new(foreground: Color) -> Self {
        Self {
            font_weight: noto_sans_mono_bitmap::FontWeight::Regular,
            font_size: noto_sans_mono_bitmap::RasterHeight::Size24,
            foreground,
            background: None,
        }
    }

    /// Set the font weight
    pub const fn with_weight(mut self, weight: FontWeight) -> Self {
        self.font_weight = weight;
        self
    }

    /// Set the font size
    pub const fn with_size(mut self, size: RasterHeight) -> Self {
        self.font_size = size;
        self
    }

    /// Set the background color (for opaque text rendering)
    pub const fn with_background(mut self, background: Color) -> Self {
        self.background = Some(background);
        self
    }
}

impl Default for TextStyle {
    #[inline]
    fn default() -> Self {
        Self::new(Color::WHITE)
    }
}

/// Text rendering metrics and character information
#[derive(Debug, Clone, Copy)]
pub struct TextMetrics {
    pub char_width: u64,
    pub char_height: u64,
    pub line_height: u64,
    pub baseline: u64,
}

impl TextMetrics {
    /// Get text metrics for a specific font size
    pub fn for_size(size: RasterHeight) -> Self {
        let char_width = get_raster_width(noto_sans_mono_bitmap::FontWeight::Regular, size) as u64;
        let char_height = size as u64;
        let line_height = char_height + 2; // Add small spacing between lines
        let baseline = (char_height * 3) / 4; // Approximate baseline position

        Self {
            char_width,
            char_height,
            line_height,
            baseline,
        }
    }

    /// Calculate the bounds of a text string
    pub fn measure_string(&self, text: &str) -> Rect {
        let lines: Vec<&str> = text.lines().collect();
        let max_width = lines
            .iter()
            .map(|line| line.chars().count() as u64 * self.char_width)
            .max()
            .unwrap_or(0);
        let height = lines.len() as u64 * self.line_height;

        Rect::new(0, 0, max_width, height)
    }
}

/// Render a character at the specified position using bitmap font
#[inline]
fn render_character_at(
    pixels: &mut [u32],
    buffer_width: u64,
    buffer_height: u64,
    x: u64,
    y: u64,
    character: char,
    style: &TextStyle,
) -> Result<()> {
    // Get the rasterized character data
    let raster = match get_raster(character, style.font_weight, style.font_size) {
        Some(raster) => raster,
        None => return Err(GraphicsError::UnsupportedCharacter),
    };

    let char_width = raster.width() as u64;
    let char_height = raster.height() as u64;

    // Check bounds
    if x + char_width > buffer_width || y + char_height > buffer_height {
        return Err(GraphicsError::OutOfBounds);
    }

    let raster_data = raster.raster();

    // Render each pixel of the character
    for char_y in 0..char_height {
        for char_x in 0..char_width {
            let pixel_x = x + char_x;
            let pixel_y = y + char_y;
            let buffer_index = (pixel_y * buffer_width + pixel_x) as usize;

            if buffer_index >= pixels.len() {
                continue;
            }

            // Get the intensity from the bitmap (0-255)
            let intensity = raster_data[char_y as usize][char_x as usize];

            if intensity == 0 {
                // Transparent pixel - only render background if specified
                if let Some(bg_color) = style.background {
                    pixels[buffer_index] = bg_color.raw();
                }
            } else {
                // Blend foreground based on intensity
                let current_color = Color::from(pixels[buffer_index]);
                let blended_color =
                    blend_text_pixel(current_color, style.foreground, intensity, style.background);
                pixels[buffer_index] = blended_color.raw();
            }
        }
    }

    Ok(())
}

/// Blend a text pixel with the background based on bitmap intensity
#[inline]
fn blend_text_pixel(
    background: Color,
    foreground: Color,
    intensity: u8,
    explicit_bg: Option<Color>,
) -> Color {
    // Use explicit background if provided, otherwise use current pixel
    let bg_color = explicit_bg.unwrap_or(background);

    if intensity == 255 {
        // Fully opaque - use foreground color
        foreground
    } else if intensity == 0 {
        // Fully transparent - use background color
        bg_color
    } else {
        // Blend based on intensity
        let alpha = intensity as u32;
        let inv_alpha = 255 - alpha;

        let r = (foreground.red() as u32 * alpha + bg_color.red() as u32 * inv_alpha) / 255;
        let g = (foreground.green() as u32 * alpha + bg_color.green() as u32 * inv_alpha) / 255;
        let b = (foreground.blue() as u32 * alpha + bg_color.blue() as u32 * inv_alpha) / 255;

        Color::from_rgb(r as u8, g as u8, b as u8)
    }
}

/// Render a string at the specified position
fn render_string_at(
    pixels: &mut [u32],
    buffer_width: u64,
    buffer_height: u64,
    x: u64,
    y: u64,
    text: &str,
    style: &TextStyle,
) -> Result<()> {
    let metrics = TextMetrics::for_size(style.font_size);
    let mut current_x = x;
    let mut current_y = y;

    for character in text.chars() {
        if character == '\n' {
            // Move to next line
            current_x = x;
            current_y += metrics.line_height;
            continue;
        }

        if character == '\r' {
            // Carriage return - just reset x position
            current_x = x;
            continue;
        }

        // Check if character would fit horizontally
        if current_x + metrics.char_width > buffer_width {
            // Word wrapping - move to next line
            current_x = x;
            current_y += metrics.line_height;
        }

        // Check if we're still within vertical bounds
        if current_y + metrics.char_height > buffer_height {
            break; // Stop rendering if we're out of vertical space
        }

        // Render the character
        render_character_at(
            pixels,
            buffer_width,
            buffer_height,
            current_x,
            current_y,
            character,
            style,
        )?;

        // Move to next character position
        current_x += metrics.char_width;
    }

    Ok(())
}

/// Render multi-line text with word wrapping
fn render_text_wrapped(
    pixels: &mut [u32],
    buffer_width: u64,
    buffer_height: u64,
    x: u64,
    y: u64,
    text: &str,
    style: &TextStyle,
    wrap_width: u64,
) -> Result<()> {
    let metrics = TextMetrics::for_size(style.font_size);
    let mut current_x = x;
    let mut current_y = y;

    let words: Vec<&str> = text.split_whitespace().collect();

    for word in words {
        let word_width = word.chars().count() as u64 * metrics.char_width;

        // Check if word fits on current line
        if current_x != x && current_x + word_width > x + wrap_width {
            // Move to next line
            current_x = x;
            current_y += metrics.line_height;
        }

        // Check vertical bounds
        if current_y + metrics.char_height > buffer_height {
            break;
        }

        // Render the word
        render_string_at(
            pixels,
            buffer_width,
            buffer_height,
            current_x,
            current_y,
            word,
            style,
        )?;

        // Move past the word and add space
        current_x += word_width + metrics.char_width; // Add space width
    }

    Ok(())
}

/// Texture struct for pixel buffer operations
#[derive(Debug, Clone)]
pub struct Texture {
    pub pixels: Vec<u32>,
    pub width: u64,
    pub height: u64,
}

impl Texture {
    /// Create a new empty texture with specified dimensions
    pub fn new(width: u64, height: u64) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(GraphicsError::InvalidInput);
        }

        let pixel_count = width
            .checked_mul(height)
            .ok_or(GraphicsError::InvalidInput)? as usize;

        let pixels = vec![0; pixel_count];

        Ok(Self {
            pixels,
            width,
            height,
        })
    }

    /// Create a texture from existing pixel buffer
    pub fn from_buffer(width: u64, height: u64, pixels: Vec<u32>) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(GraphicsError::InvalidInput);
        }

        let expected_len = width
            .checked_mul(height)
            .ok_or(GraphicsError::InvalidInput)? as usize;

        if pixels.len() != expected_len {
            return Err(GraphicsError::InvalidInput);
        }

        Ok(Self {
            pixels,
            width,
            height,
        })
    }

    /// Set a pixel at the given coordinates
    pub fn set_pixel(&mut self, x: u64, y: u64, color: Color) -> Result<()> {
        if x >= self.width || y >= self.height {
            return Err(GraphicsError::OutOfBounds);
        }

        let index = (y * self.width + x) as usize;
        if index >= self.pixels.len() {
            return Err(GraphicsError::OutOfBounds);
        }

        self.pixels[index] = color.raw();
        Ok(())
    }

    /// Get a pixel color at the given coordinates
    pub fn get_pixel(&self, x: u64, y: u64) -> Result<Color> {
        if x >= self.width || y >= self.height {
            return Err(GraphicsError::OutOfBounds);
        }

        let index = (y * self.width + x) as usize;
        if index >= self.pixels.len() {
            return Err(GraphicsError::OutOfBounds);
        }

        Ok(Color::from(self.pixels[index]))
    }

    /// Fill a rectangular area with a color
    pub fn fill_rect(
        &mut self,
        x: u64,
        y: u64,
        width: u64,
        height: u64,
        color: Color,
    ) -> Result<()> {
        if x >= self.width || y >= self.height {
            return Err(GraphicsError::OutOfBounds);
        }

        let end_x = (x + width).min(self.width);
        let end_y = (y + height).min(self.height);

        for py in y..end_y {
            for px in x..end_x {
                self.set_pixel(px, py, color)?;
            }
        }
        Ok(())
    }

    /// Fill the entire texture with a color
    pub fn fill(&mut self, color: Color) {
        let raw_color = color.raw();
        for pixel in &mut self.pixels {
            *pixel = raw_color;
        }
    }

    /// Clear the texture (set all pixels to black)
    pub fn clear(&mut self) {
        self.fill(Color::BLACK);
    }

    /// Render a character at the specified position
    pub fn draw_char(&mut self, x: u64, y: u64, character: char, style: &TextStyle) -> Result<()> {
        render_character_at(
            &mut self.pixels,
            self.width,
            self.height,
            x,
            y,
            character,
            style,
        )
    }

    /// Render text at the specified position
    pub fn draw_text(&mut self, x: u64, y: u64, text: &str, style: &TextStyle) -> Result<()> {
        render_string_at(&mut self.pixels, self.width, self.height, x, y, text, style)
    }

    /// Render text with word wrapping within the specified width
    pub fn draw_text_wrapped(
        &mut self,
        x: u64,
        y: u64,
        text: &str,
        style: &TextStyle,
        wrap_width: u64,
    ) -> Result<()> {
        render_text_wrapped(
            &mut self.pixels,
            self.width,
            self.height,
            x,
            y,
            text,
            style,
            wrap_width,
        )
    }

    /// Get text metrics for a given style
    pub fn text_metrics(style: &TextStyle) -> TextMetrics {
        TextMetrics::for_size(style.font_size)
    }

    /// Measure the bounds of text without rendering it
    pub fn measure_text(text: &str, style: &TextStyle) -> Rect {
        let metrics = TextMetrics::for_size(style.font_size);
        metrics.measure_string(text)
    }
}

/// Blending modes for texture operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    /// Replace destination with source
    Replace,
    /// Blend based on RGB intensity (lighter colors have more influence)
    IntensityBlend,
    /// Add source to destination
    Add,
    /// Multiply source with destination
    Multiply,
}

impl BlendMode {
    /// Apply blending between source and destination colors
    pub fn blend(self, src: Color, dst: Color) -> Color {
        match self {
            BlendMode::Replace => src,
            BlendMode::IntensityBlend => {
                // Calculate intensity (brightness) of source color
                let src_intensity = (src.red() as u32 + src.green() as u32 + src.blue() as u32) / 3;
                let dst_intensity = (dst.red() as u32 + dst.green() as u32 + dst.blue() as u32) / 3;

                // Blend factor based on relative intensities (0-255)
                let total_intensity = src_intensity + dst_intensity;
                if total_intensity == 0 {
                    Color::from_rgb(0, 0, 0)
                } else {
                    let src_factor = (src_intensity * 255) / total_intensity;
                    let dst_factor = 255 - src_factor;

                    let r = (src.red() as u32 * src_factor + dst.red() as u32 * dst_factor) / 255;
                    let g =
                        (src.green() as u32 * src_factor + dst.green() as u32 * dst_factor) / 255;
                    let b = (src.blue() as u32 * src_factor + dst.blue() as u32 * dst_factor) / 255;

                    Color::from_rgb(r as u8, g as u8, b as u8)
                }
            }
            BlendMode::Add => {
                let r = (src.red() as u32 + dst.red() as u32).min(255);
                let g = (src.green() as u32 + dst.green() as u32).min(255);
                let b = (src.blue() as u32 + dst.blue() as u32).min(255);

                Color::from_rgb(r as u8, g as u8, b as u8)
            }
            BlendMode::Multiply => {
                let r = (src.red() as u32 * dst.red() as u32) / 255;
                let g = (src.green() as u32 * dst.green() as u32) / 255;
                let b = (src.blue() as u32 * dst.blue() as u32) / 255;

                Color::from_rgb(r as u8, g as u8, b as u8)
            }
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ScreenInfo {
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone)]
pub struct DrawRequest {
    pub pixels: Vec<u32>,
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
}

impl DrawRequest {
    /// Create a new DrawRequest with specified dimensions at origin (0,0)
    pub fn new(width: u64, height: u64) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(GraphicsError::InvalidInput);
        }

        let pixel_count = width
            .checked_mul(height)
            .ok_or(GraphicsError::InvalidInput)? as usize;

        let pixels = vec![0; pixel_count];

        Ok(Self {
            pixels,
            x: 0,
            y: 0,
            width,
            height,
        })
    }

    /// Set the position where this DrawRequest will be drawn
    pub fn with_position(mut self, x: u64, y: u64) -> Self {
        self.x = x;
        self.y = y;
        self
    }

    /// Set a pixel at the given coordinates within this DrawRequest
    #[inline(always)]
    pub fn set_pixel(&mut self, x: u64, y: u64, color: Color) -> Result<()> {
        if x >= self.width || y >= self.height {
            return Err(GraphicsError::OutOfBounds);
        }

        let index = (y * self.width + x) as usize;
        if index >= self.pixels.len() {
            return Err(GraphicsError::OutOfBounds);
        }

        self.pixels[index] = color.raw();
        Ok(())
    }

    /// Get a pixel color at the given coordinates
    pub fn get_pixel(&self, x: u64, y: u64) -> Result<Color> {
        if x >= self.width || y >= self.height {
            return Err(GraphicsError::OutOfBounds);
        }

        let index = (y * self.width + x) as usize;
        if index >= self.pixels.len() {
            return Err(GraphicsError::OutOfBounds);
        }

        Ok(Color::from(self.pixels[index]))
    }

    /// Fill a rectangular area with a color
    pub fn fill_rect(
        &mut self,
        x: u64,
        y: u64,
        width: u64,
        height: u64,
        color: Color,
    ) -> Result<()> {
        if x >= self.width || y >= self.height {
            return Err(GraphicsError::OutOfBounds);
        }

        let end_x = (x + width).min(self.width);
        let end_y = (y + height).min(self.height);

        for py in y..end_y {
            for px in x..end_x {
                self.set_pixel(px, py, color)?;
            }
        }
        Ok(())
    }

    /// Fill the entire DrawRequest with a color
    pub fn fill(&mut self, color: Color) {
        let raw_color = color.raw();
        for pixel in &mut self.pixels {
            *pixel = raw_color;
        }
    }

    /// Clear the DrawRequest (set all pixels to black)
    pub fn clear(&mut self) {
        self.fill(Color::BLACK);
    }

    /// Draw a line from (x1, y1) to (x2, y2) using Bresenham's algorithm
    pub fn draw_line(&mut self, x1: u64, y1: u64, x2: u64, y2: u64, color: Color) -> Result<()> {
        let mut x1 = x1 as i64;
        let mut y1 = y1 as i64;
        let x2 = x2 as i64;
        let y2 = y2 as i64;

        let dx = (x2 - x1).abs();
        let dy = (y2 - y1).abs();
        let sx = if x1 < x2 { 1 } else { -1 };
        let sy = if y1 < y2 { 1 } else { -1 };
        let mut err = dx - dy;

        loop {
            if x1 >= 0 && y1 >= 0 && x1 < self.width as i64 && y1 < self.height as i64 {
                self.set_pixel(x1 as u64, y1 as u64, color)?;
            }

            if x1 == x2 && y1 == y2 {
                break;
            }

            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x1 += sx;
            }
            if e2 < dx {
                err += dx;
                y1 += sy;
            }
        }
        Ok(())
    }

    /// Draw a circle outline at (cx, cy) with given radius
    pub fn draw_circle(&mut self, cx: u64, cy: u64, radius: u64, color: Color) -> Result<()> {
        let cx = cx as i64;
        let cy = cy as i64;
        let radius = radius as i64;

        let mut x = radius;
        let mut y = 0i64;
        let mut err = 0i64;

        while x >= y {
            // Plot 8 octants
            let points = [
                (cx + x, cy + y),
                (cx + y, cy + x),
                (cx - y, cy + x),
                (cx - x, cy + y),
                (cx - x, cy - y),
                (cx - y, cy - x),
                (cx + y, cy - x),
                (cx + x, cy - y),
            ];

            for (px, py) in points.iter() {
                if *px >= 0 && *py >= 0 && (*px as u64) < self.width && (*py as u64) < self.height {
                    self.set_pixel(*px as u64, *py as u64, color)?;
                }
            }

            if err <= 0 {
                y += 1;
                err += 2 * y + 1;
            }
            if err > 0 {
                x -= 1;
                err -= 2 * x + 1;
            }
        }
        Ok(())
    }

    /// Fill a circle at (cx, cy) with given radius
    pub fn fill_circle(&mut self, cx: u64, cy: u64, radius: u64, color: Color) -> Result<()> {
        let cx = cx as i64;
        let cy = cy as i64;
        let radius = radius as i64;

        for y in (cy - radius)..=(cy + radius) {
            if y < 0 || y >= self.height as i64 {
                continue;
            }

            let dy = y - cy;
            let dy_squared = dy * dy;
            let under_sqrt = radius * radius - dy_squared;

            if under_sqrt < 0 {
                continue;
            }

            let dx = (under_sqrt as u64).isqrt() as i64;

            let start = (cx - dx).max(0) as u64;
            let end = (cx + dx).min(self.width as i64 - 1) as u64;

            if start <= end {
                self.fill_rect(start, y as u64, end - start + 1, 1, color)?;
            }
        }
        Ok(())
    }

    /// Blit a texture to this DrawRequest at the specified position
    pub fn blit_texture(&mut self, texture: &Texture, dst_x: u64, dst_y: u64) -> Result<()> {
        self.blit_texture_with_blend(texture, None, dst_x, dst_y, BlendMode::Replace)
    }

    /// Blit a texture region to this DrawRequest at the specified position
    pub fn blit_texture_region(
        &mut self,
        texture: &Texture,
        src_rect: Rect,
        dst_x: u64,
        dst_y: u64,
    ) -> Result<()> {
        self.blit_texture_with_blend(texture, Some(src_rect), dst_x, dst_y, BlendMode::Replace)
    }

    /// Blit a texture with intensity blending
    pub fn blit_texture_intensity(
        &mut self,
        texture: &Texture,
        dst_x: u64,
        dst_y: u64,
    ) -> Result<()> {
        self.blit_texture_with_blend(texture, None, dst_x, dst_y, BlendMode::IntensityBlend)
    }

    /// Blit a texture with custom blending mode
    pub fn blit_texture_with_blend(
        &mut self,
        texture: &Texture,
        src_rect: Option<Rect>,
        dst_x: u64,
        dst_y: u64,
        blend_mode: BlendMode,
    ) -> Result<()> {
        // Determine source region
        let (src_x, src_y, src_width, src_height) = match src_rect {
            Some(rect) => (rect.x, rect.y, rect.width, rect.height),
            None => (0, 0, texture.width, texture.height),
        };

        // Bounds check source region
        if src_x >= texture.width || src_y >= texture.height {
            return Err(GraphicsError::OutOfBounds);
        }

        let src_end_x = (src_x + src_width).min(texture.width);
        let src_end_y = (src_y + src_height).min(texture.height);

        // Copy pixels with blending
        for src_py in src_y..src_end_y {
            for src_px in src_x..src_end_x {
                let dst_px = dst_x + (src_px - src_x);
                let dst_py = dst_y + (src_py - src_y);

                // Skip if destination is out of bounds
                if dst_px >= self.width || dst_py >= self.height {
                    continue;
                }

                let src_color = texture.get_pixel(src_px, src_py)?;

                match blend_mode {
                    BlendMode::Replace => {
                        self.set_pixel(dst_px, dst_py, src_color)?;
                    }
                    _ => {
                        let dst_color = self.get_pixel(dst_px, dst_py)?;
                        let blended = blend_mode.blend(src_color, dst_color);
                        self.set_pixel(dst_px, dst_py, blended)?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Blit raw pixel data to this DrawRequest
    pub fn blit_pixels(
        &mut self,
        pixels: &[u32],
        width: u64,
        height: u64,
        dst_x: u64,
        dst_y: u64,
    ) -> Result<()> {
        let texture = Texture::from_buffer(width, height, pixels.to_vec())?;
        self.blit_texture(&texture, dst_x, dst_y)
    }

    /// Blit with scaling (simple nearest neighbor)
    pub fn blit_texture_scaled(
        &mut self,
        texture: &Texture,
        src_rect: Option<Rect>,
        dst_rect: Rect,
    ) -> Result<()> {
        let (src_x, src_y, src_width, src_height) = match src_rect {
            Some(rect) => (rect.x, rect.y, rect.width, rect.height),
            None => (0, 0, texture.width, texture.height),
        };

        if src_width == 0 || src_height == 0 || dst_rect.width == 0 || dst_rect.height == 0 {
            return Ok(());
        }

        // Simple nearest neighbor scaling
        for dst_py in 0..dst_rect.height {
            for dst_px in 0..dst_rect.width {
                let src_px = src_x + (dst_px * src_width / dst_rect.width);
                let src_py = src_y + (dst_py * src_height / dst_rect.height);

                if src_px < texture.width && src_py < texture.height {
                    let color = texture.get_pixel(src_px, src_py)?;
                    let final_x = dst_rect.x + dst_px;
                    let final_y = dst_rect.y + dst_py;

                    if final_x < self.width && final_y < self.height {
                        self.set_pixel(final_x, final_y, color)?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Render a character at the specified position
    pub fn draw_char(&mut self, x: u64, y: u64, character: char, style: &TextStyle) -> Result<()> {
        render_character_at(
            &mut self.pixels,
            self.width,
            self.height,
            x,
            y,
            character,
            style,
        )
    }

    /// Render text at the specified position
    pub fn draw_text(&mut self, x: u64, y: u64, text: &str, style: &TextStyle) -> Result<()> {
        render_string_at(&mut self.pixels, self.width, self.height, x, y, text, style)
    }

    /// Render text with word wrapping within the specified width
    pub fn draw_text_wrapped(
        &mut self,
        x: u64,
        y: u64,
        text: &str,
        style: &TextStyle,
        wrap_width: u64,
    ) -> Result<()> {
        render_text_wrapped(
            &mut self.pixels,
            self.width,
            self.height,
            x,
            y,
            text,
            style,
            wrap_width,
        )
    }
}

/// Screen management struct providing convenient graphics operations
pub struct Screen {
    framebuffer: Framebuffer,
    info: ScreenInfo,
    back_buffer: Option<DrawRequest>,
    dirty: bool,
}

impl Screen {
    /// Get the global screen instance
    pub fn get() -> Result<Screen> {
        let fb = Framebuffer::new();
        let info = fb.screen_info()?;
        Ok(Screen {
            framebuffer: fb,
            info,
            back_buffer: None,
            dirty: false,
        })
    }

    /// Get screen width
    pub fn width(&self) -> usize {
        self.info.width
    }

    /// Get screen height
    pub fn height(&self) -> usize {
        self.info.height
    }

    /// Get screen info
    pub fn info(&self) -> &ScreenInfo {
        &self.info
    }

    /// Ensure the back buffer is initialized
    fn ensure_back_buffer(&mut self) -> Result<()> {
        if self.back_buffer.is_none() {
            let mut buffer = DrawRequest::new(self.width() as u64, self.height() as u64)?;
            buffer.clear(); // Initialize with black background
            self.back_buffer = Some(buffer);
        }
        Ok(())
    }

    /// Draw a rectangle on the screen
    pub fn draw_rect(
        &mut self,
        x: u64,
        y: u64,
        width: u64,
        height: u64,
        color: Color,
    ) -> Result<()> {
        self.ensure_back_buffer()?;

        if let Some(ref mut buffer) = self.back_buffer {
            buffer.fill_rect(x, y, width, height, color)?;
            self.dirty = true;
        }

        Ok(())
    }

    /// Fill the entire screen with a color
    pub fn fill(&mut self, color: Color) -> Result<()> {
        self.ensure_back_buffer()?;

        if let Some(ref mut buffer) = self.back_buffer {
            buffer.fill(color);
            self.dirty = true;
        }

        Ok(())
    }

    /// Clear the screen (fill with black)
    pub fn clear(&mut self) -> Result<()> {
        self.fill(Color::BLACK)
    }

    /// Render all pending operations
    pub fn render(&mut self) -> Result<()> {
        // Only render if we have a dirty back buffer
        if self.dirty
            && let Some(ref buffer) = self.back_buffer
        {
            self.framebuffer.draw(buffer)?;
            self.dirty = false;
        }

        // Always call the final render syscall to present to screen
        self.framebuffer.render();
        Ok(())
    }

    /// Create a new DrawRequest that fits entirely on screen
    pub fn create_draw_request(&self, width: u64, height: u64) -> Result<DrawRequest> {
        if width > self.width() as u64 || height > self.height() as u64 {
            return Err(GraphicsError::OutOfBounds);
        }
        DrawRequest::new(width, height)
    }

    /// Draw a texture directly to the screen at the specified position
    pub fn draw_texture(&mut self, texture: &Texture, x: u64, y: u64) -> Result<()> {
        let mut draw_req = DrawRequest::new(texture.width, texture.height)?;
        draw_req.blit_texture(texture, 0, 0)?;
        draw_req = draw_req.with_position(x, y);
        self.framebuffer.draw(&draw_req)
    }

    /// Draw a texture region directly to the screen
    pub fn draw_texture_region(
        &mut self,
        texture: &Texture,
        src_rect: Rect,
        x: u64,
        y: u64,
    ) -> Result<()> {
        let mut draw_req = DrawRequest::new(src_rect.width, src_rect.height)?;
        draw_req.blit_texture_region(texture, src_rect, 0, 0)?;
        draw_req = draw_req.with_position(x, y);
        self.framebuffer.draw(&draw_req)
    }

    /// Draw a texture with intensity blending directly to the screen
    pub fn draw_texture_intensity(&mut self, texture: &Texture, x: u64, y: u64) -> Result<()> {
        let mut draw_req = DrawRequest::new(texture.width, texture.height)?;
        draw_req.blit_texture_intensity(texture, 0, 0)?;
        draw_req = draw_req.with_position(x, y);
        self.framebuffer.draw(&draw_req)
    }

    /// Draw a texture with scaling directly to the screen
    pub fn draw_texture_scaled(&mut self, texture: &Texture, dst_rect: Rect) -> Result<()> {
        let mut draw_req = DrawRequest::new(dst_rect.width, dst_rect.height)?;
        draw_req.blit_texture_scaled(
            texture,
            None,
            Rect::new(0, 0, dst_rect.width, dst_rect.height),
        )?;
        draw_req = draw_req.with_position(dst_rect.x, dst_rect.y);
        self.framebuffer.draw(&draw_req)
    }

    /// Create a texture from the screen contents (screenshot)
    pub fn capture_region(&self, _region: Rect) -> Result<Texture> {
        // This would require a new syscall to read from framebuffer
        // For now, return an error as this functionality isn't implemented
        Err(GraphicsError::Unknown)
    }

    /// Draw text directly to the screen
    pub fn draw_text(&mut self, x: u64, y: u64, text: &str, style: &TextStyle) -> Result<()> {
        self.ensure_back_buffer()?;

        if let Some(ref mut buffer) = self.back_buffer {
            buffer.draw_text(x, y, text, style)?;
            self.dirty = true;
        }

        Ok(())
    }

    /// Draw text with word wrapping directly to the screen
    pub fn draw_text_wrapped(
        &mut self,
        x: u64,
        y: u64,
        text: &str,
        style: &TextStyle,
        wrap_width: u64,
    ) -> Result<()> {
        self.ensure_back_buffer()?;

        if let Some(ref mut buffer) = self.back_buffer {
            buffer.draw_text_wrapped(x, y, text, style, wrap_width)?;
            self.dirty = true;
        }

        Ok(())
    }

    /// Draw a single character directly to the screen
    pub fn draw_char(&mut self, x: u64, y: u64, character: char, style: &TextStyle) -> Result<()> {
        self.ensure_back_buffer()?;

        if let Some(ref mut buffer) = self.back_buffer {
            buffer.draw_char(x, y, character, style)?;
            self.dirty = true;
        }

        Ok(())
    }
}

/// Get the global screen instance (convenience function)
pub fn screen() -> Result<Screen> {
    Screen::get()
}
