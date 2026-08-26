use std::{fs::File, os::edos::io::FileExt};

// Re-export noto-sans-mono-bitmap types for user programs
use edos_lib::sys::{SYS_MMAP, is_err, syscall5};
pub use noto_sans_mono_bitmap::{FontWeight, RasterHeight};
use noto_sans_mono_bitmap::{get_raster, get_raster_width};
use std::fmt;

/// Graphics operation error type
#[derive(Debug, Clone, Copy)]
pub enum GraphicsError {
    InvalidInput,
    OutOfMemory,
    Fault,
    Unknown,
    InvalidColor,
    OutOfBounds,
    UnsupportedCharacter,
    TextError,
}

impl fmt::Display for GraphicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput => write!(f, "Invalid argument or coordinates"),
            Self::OutOfMemory => write!(f, "Out of memory"),
            Self::Fault => write!(f, "Bad address/fault"),
            Self::Unknown => write!(f, "Unknown graphics error"),
            Self::InvalidColor => write!(f, "Invalid color value"),
            Self::OutOfBounds => write!(f, "Coordinates out of bounds"),
            Self::UnsupportedCharacter => write!(f, "Unsupported character"),
            Self::TextError => write!(f, "Text rendering error"),
        }
    }
}

impl std::error::Error for GraphicsError {}

// Ioctl buffer flags
pub const IOCTL_ARG_IN: u64 = 1;
pub const IOCTL_ARG_OUT: u64 = 1 << 1;

const FB_IOCTL_DRAW_RECT: u64 = 0x4642_0001;
const FB_IOCTL_RENDER: u64 = 0x4642_0002;
const FB_IOCTL_DRAW: u64 = 0x4642_0003;
const FB_IOCTL_SCREEN_INFO: u64 = 0x4642_0004;
const FB_IOCTL_FLIP: u64 = 0x4642_0005;
const FB_IOCTL_MMAP_INFO: u64 = 0x4642_0006;
const FB_IOCTL_FLIP_RECT: u64 = 0x4642_0009;
const FB_IOCTL_FLIP_WAIT: u64 = 0x4642_000A;
const FB_IOCTL_FLIP_RECTS: u64 = 0x4642_000B;

/// The most regions one frame may be split into. Mirrors the kernel's
/// `graphics::MAX_FLIP_RECTS`; the two are one ABI and change together.
pub const MAX_FLIP_RECTS: usize = 16;
const FB_IOCTL_SET_CURSOR: u64 = 0x4642_0007;
const FB_IOCTL_MOVE_CURSOR: u64 = 0x4642_0008;
const FB_IOCTL_TRACK_POINTER: u64 = 0x4642_000C;

const PROT_READ: u32 = 0x1;
const PROT_WRITE: u32 = 0x2;
const MAP_PRIVATE: u32 = 0x02;
const MAP_PHYSICAL: u32 = 0x40;
const MAP_WRITE_COMBINING: u32 = 0x80;

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
    refresh_rate: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FramebufferMmapInfo {
    pub phys_addr: u64,
    pub total_size: u64,
    pub page_size: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub double_buffered: u8,
    pub is_identity: u8,
    pub _padding: u16,
}

#[derive(Debug)]
pub struct Framebuffer {
    fd: File,
    buffer: Vec<u8>,
}

impl Framebuffer {
    pub fn new() -> Self {
        let file = File::open("/dev/fb").unwrap_or_else(|e| {
            panic!("Framebuffer: failed to open /dev/fb: {e}");
        });
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

    /// Flip the display (full screen transfer).
    pub fn flip(&self) -> u64 {
        self.fd.ioctl(FB_IOCTL_FLIP, 0, 0, 0).unwrap()
    }

    /// Flip only a dirty rectangle (partial transfer).
    /// Wait for the previous flip's pixels to be read out of the framebuffer.
    ///
    /// The flip is asynchronous, so the display may still be copying the last
    /// frame out of the buffer the next one is about to be written into. This
    /// is called immediately before that write, which is the only place it is
    /// any use: a whole compositing pass has happened since the flip was
    /// submitted, so in the ordinary case there is nothing left to wait for.
    pub fn flip_wait(&self) {
        let _ = self.fd.ioctl(FB_IOCTL_FLIP_WAIT, 0, 0, IOCTL_ARG_IN);
    }

    /// Publish several disjoint regions as one frame.
    ///
    /// One flush behind several transfers, so the display copies only the
    /// pixels that changed while the frame still arrives whole. `bounds` must
    /// cover every rect.
    pub fn flip_rects(&self, rects: &[(u32, u32, u32, u32)], bounds: (u32, u32, u32, u32)) -> u64 {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct FlipRect {
            x: u32,
            y: u32,
            width: u32,
            height: u32,
        }
        #[repr(C)]
        struct FlipRects {
            count: u32,
            bounds: FlipRect,
            rects: [FlipRect; MAX_FLIP_RECTS],
        }
        let zero = FlipRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        let n = rects.len().min(MAX_FLIP_RECTS);
        let mut list = FlipRects {
            count: n as u32,
            bounds: FlipRect {
                x: bounds.0,
                y: bounds.1,
                width: bounds.2,
                height: bounds.3,
            },
            rects: [zero; MAX_FLIP_RECTS],
        };
        for (slot, &(x, y, w, h)) in list.rects.iter_mut().zip(&rects[..n]) {
            *slot = FlipRect {
                x,
                y,
                width: w,
                height: h,
            };
        }
        self.fd
            .ioctl(
                FB_IOCTL_FLIP_RECTS,
                (&mut list as *mut FlipRects) as u64,
                core::mem::size_of::<FlipRects>(),
                IOCTL_ARG_IN,
            )
            .unwrap_or(0)
    }

    pub fn flip_rect(&self, x: u32, y: u32, w: u32, h: u32) -> u64 {
        #[repr(C)]
        struct FlipRect {
            x: u32,
            y: u32,
            width: u32,
            height: u32,
        }
        let mut r = FlipRect {
            x,
            y,
            width: w,
            height: h,
        };
        self.fd
            .ioctl(
                FB_IOCTL_FLIP_RECT,
                (&mut r as *mut FlipRect) as u64,
                core::mem::size_of::<FlipRect>(),
                IOCTL_ARG_IN,
            )
            .unwrap()
    }

    /// Get framebuffer mmap info for direct VRAM access.
    pub fn mmap_info(&self) -> Result<FramebufferMmapInfo> {
        let mut info = FramebufferMmapInfo::default();
        self.fd
            .ioctl(
                FB_IOCTL_MMAP_INFO,
                (&mut info as *mut FramebufferMmapInfo) as u64,
                core::mem::size_of::<FramebufferMmapInfo>(),
                IOCTL_ARG_OUT,
            )
            .map_err(|_| GraphicsError::Unknown)?;
        Ok(info)
    }

    /// Map the framebuffer VRAM directly into userspace.
    pub fn mmap_vram(&self) -> Result<VramMapping> {
        let info = self.mmap_info()?;

        if info.is_identity == 0 {
            return Err(GraphicsError::Unknown);
        }

        let ptr = unsafe {
            syscall5(
                SYS_MMAP,
                0,
                info.total_size,
                (PROT_READ | PROT_WRITE) as u64,
                (MAP_PRIVATE | MAP_PHYSICAL | MAP_WRITE_COMBINING) as u64,
                info.phys_addr,
            )
        } as *mut u32;

        if ptr.is_null() || is_err(ptr as u64) {
            return Err(GraphicsError::Fault);
        }

        let page_pixels = info.page_size as usize / core::mem::size_of::<u32>();
        let pitch_pixels = info.pitch as usize / core::mem::size_of::<u32>();

        // The kernel starts with back_page_y_offset = height (page 1 is the
        // back page, page 0 is displayed). Match that initial state.
        let initial_back_offset = if info.double_buffered != 0 {
            (info.height as usize) * pitch_pixels
        } else {
            0
        };

        Ok(VramMapping {
            base: ptr,
            total_size: info.total_size as usize,
            page_pixels,
            width: info.width as usize,
            height: info.height as usize,
            pitch_pixels,
            double_buffered: info.double_buffered != 0,
            back_offset: initial_back_offset,
        })
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
            .map_err(|e| {
                eprintln!("Framebuffer: screen_info ioctl failed: {e}");
                GraphicsError::Unknown
            })?;

        Ok(ScreenInfo {
            width: info.width as usize,
            height: info.height as usize,
            refresh_rate: info.refresh_rate,
        })
    }

    /// Set the hardware cursor image. `pixels` is WxH ARGB values.
    /// Returns true if the hardware cursor was set, false if unsupported.
    pub fn set_cursor(
        &self,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        pixels: &[u32],
    ) -> bool {
        #[repr(C)]
        struct SetCursorHeader {
            width: u32,
            height: u32,
            hot_x: u32,
            hot_y: u32,
            pixel_count: u32,
            _padding: u32,
        }

        let header = SetCursorHeader {
            width,
            height,
            hot_x,
            hot_y,
            pixel_count: pixels.len() as u32,
            _padding: 0,
        };

        // Build buffer: header + pixel data
        let header_bytes = core::mem::size_of::<SetCursorHeader>();
        let total = header_bytes + pixels.len() * 4;
        let mut buf = vec![0u8; total];
        unsafe {
            core::ptr::copy_nonoverlapping(
                &header as *const _ as *const u8,
                buf.as_mut_ptr(),
                header_bytes,
            );
            core::ptr::copy_nonoverlapping(
                pixels.as_ptr() as *const u8,
                buf.as_mut_ptr().add(header_bytes),
                pixels.len() * 4,
            );
        }

        self.fd
            .ioctl(
                FB_IOCTL_SET_CURSOR,
                buf.as_ptr() as u64,
                total,
                IOCTL_ARG_IN,
            )
            .is_ok()
    }

    /// Ask the display to keep its cursor plane on the pointer by itself.
    ///
    /// Returns whether the display took it. A caller told `true` no longer
    /// needs a `move_cursor` per frame: the kernel places the plane as each
    /// input report lands, so the pointer stops being resampled down to the
    /// compositor's frame rate. One told `false` must keep moving it.
    ///
    /// `set_cursor` re-places the plane at the origin, so a shape change still
    /// owes one `move_cursor` afterwards even while tracking.
    pub fn track_pointer(&self, enabled: bool) -> bool {
        let mut enable: u32 = enabled as u32;
        self.fd
            .ioctl(
                FB_IOCTL_TRACK_POINTER,
                (&mut enable as *mut u32) as u64,
                core::mem::size_of::<u32>(),
                IOCTL_ARG_IN,
            )
            .is_ok()
    }

    /// Move the hardware cursor. Very cheap (no frame redraw needed).
    pub fn move_cursor(&self, x: u32, y: u32) {
        #[repr(C)]
        struct MoveCursor {
            x: u32,
            y: u32,
        }
        let mut cmd = MoveCursor { x, y };
        let _ = self.fd.ioctl(
            FB_IOCTL_MOVE_CURSOR,
            (&mut cmd as *mut MoveCursor) as u64,
            core::mem::size_of::<MoveCursor>(),
            IOCTL_ARG_IN,
        );
    }

    /// Draw a raw pixel slice to the screen at (x, y) with dimensions (width, height).
    /// The caller must ensure `pixels.len() == width * height`.
    pub fn draw_pixels(
        &mut self,
        x: u64,
        y: u64,
        width: u64,
        height: u64,
        pixels: &[u32],
    ) -> Result<()> {
        let pixel_count = width
            .checked_mul(height)
            .ok_or(GraphicsError::InvalidInput)?;

        if pixel_count == 0 {
            return Ok(());
        }

        let header = FramebufferDraw {
            x,
            y,
            width,
            height,
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
            core::slice::from_raw_parts(pixels.as_ptr() as *const u8, pixel_bytes)
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

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}

pub type Result<T> = std::result::Result<T, GraphicsError>;

/// Direct VRAM mapping for zero-copy framebuffer access.
pub struct VramMapping {
    base: *mut u32,
    #[allow(dead_code)]
    total_size: usize,
    /// Pixels per page (page_size / 4).
    page_pixels: usize,
    pub width: usize,
    pub height: usize,
    /// Row stride in u32 units (pitch / 4).
    pub pitch_pixels: usize,
    pub double_buffered: bool,
    /// Current back page offset in u32 units.
    back_offset: usize,
}

impl VramMapping {
    /// Returns a mutable slice of the current back page pixels.
    pub fn back_page(&mut self) -> &mut [u32] {
        let len = if self.double_buffered {
            self.page_pixels
        } else {
            self.width * self.height
        };
        unsafe { core::slice::from_raw_parts_mut(self.base.add(self.back_offset), len) }
    }

    /// Update the back page offset from a byte offset returned by flip.
    pub fn update_back_offset(&mut self, byte_offset: u64) {
        self.back_offset = byte_offset as usize / core::mem::size_of::<u32>();
    }

    /// The pixels of one page, for a reader that wants a page it names rather
    /// than whichever one is currently being drawn into.
    ///
    /// A single-buffered display has only page 0 and answers with it whatever
    /// is asked for.
    pub fn page(&self, index: usize) -> &[u32] {
        let (len, offset) = if self.double_buffered {
            (self.page_pixels, index.min(1) * self.page_pixels)
        } else {
            (self.width * self.height, 0)
        };
        unsafe { core::slice::from_raw_parts(self.base.add(offset), len) }
    }
}

unsafe impl Send for VramMapping {}

/// Type-safe color representation (RGB format)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(u32);

impl Color {
    /// Create a new color from RGB components (alpha defaults to 255)
    #[inline]
    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self(0xFF000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
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
        self.pixels.fill(raw_color);
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

    /// Render a line of interface text, in the shell's outline face.
    ///
    /// Separate from `draw_text`, which is the bitmap path this is gradually
    /// replacing: chrome goes through here, and anything that is genuinely a
    /// character grid stays where it is.
    pub fn draw_styled_text(&mut self, x: i32, y: i32, text: &str, style: crate::text::Style) {
        let (width, height) = (self.width as u32, self.height as u32);
        let mut surface = crate::surface::Surface::new(&mut self.pixels, width, height);
        crate::text::draw(&mut surface, x, y, text, style);
    }

    /// Width of a string in the shell's outline face.
    pub fn styled_text_width(text: &str, style: crate::text::Style) -> u32 {
        crate::text::width(text, style)
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
                match (src_intensity * 255).checked_div(total_intensity) {
                    None => Color::from_rgb(0, 0, 0),
                    Some(src_factor) => {
                        let dst_factor = 255 - src_factor;

                        let r =
                            (src.red() as u32 * src_factor + dst.red() as u32 * dst_factor) / 255;
                        let g = (src.green() as u32 * src_factor + dst.green() as u32 * dst_factor)
                            / 255;
                        let b =
                            (src.blue() as u32 * src_factor + dst.blue() as u32 * dst_factor) / 255;

                        Color::from_rgb(r as u8, g as u8, b as u8)
                    }
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
    pub refresh_rate: u32,
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
        self.pixels.fill(raw_color);
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
    /// Pre-allocated scratch buffer for render_region to avoid per-call allocation.
    region_scratch: Vec<u32>,
    vram: Option<VramMapping>,
    /// Row stride in u32 units. Equals width for Vec-backed, pitch/4 for VRAM-backed.
    pitch_pixels: usize,
    /// Drawing is confined to this rectangle, in screen pixels, when set.
    clip: Option<(usize, usize, usize, usize)>,
    /// Where drawing goes when the framebuffer is mapped VRAM.
    ///
    /// virtio-gpu reports `double_buffered: 0` and page flipping exists only on
    /// the Bochs VBE path, so the mapped VRAM *is* the scanout: drawing into it
    /// puts half-finished frames where the host can read them, and compositing
    /// a frame takes about a millisecond of scattered writes. Drawing into this
    /// instead means VRAM is only ever written by the short copy in
    /// `publish`, immediately before the region is handed to the display.
    shadow: Vec<u32>,
    /// What the page that is currently the back one has not been given yet,
    /// beyond whatever the next partial flip publishes.
    ///
    /// A partial publish writes into whichever page is back at the time, so on
    /// a flipping display each page only ever receives the rectangles sent
    /// while it held that role: the page about to be shown is a frame behind,
    /// missing what the previous frame published as well as what this one does.
    /// Publishing the union brings it level. Without it a client that paints
    /// once and never again lands on one page and not the other, and appears or
    /// does not depending on which page the first flip happens to show. It
    /// starts as the whole screen because a page nothing has been published to
    /// is missing all of it.
    pending_for_back_page: (u32, u32, u32, u32),
}

/// The smallest rectangle covering both, with an empty one contributing
/// nothing rather than pulling the result to the origin.
fn union_rect(a: (u32, u32, u32, u32), b: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
    if a.2 == 0 || a.3 == 0 {
        return b;
    }
    if b.2 == 0 || b.3 == 0 {
        return a;
    }
    let x = a.0.min(b.0);
    let y = a.1.min(b.1);
    let right = (a.0 + a.2).max(b.0 + b.2);
    let bottom = (a.1 + a.3).max(b.1 + b.3);
    (x, y, right - x, bottom - y)
}

impl Screen {
    /// Try to create a VRAM-backed screen instance.
    fn new_vram() -> Result<Screen> {
        let fb = Framebuffer::new();
        let info = fb.screen_info()?;
        let mapping = fb.mmap_vram()?;
        let pitch_pixels = mapping.pitch_pixels;
        Ok(Screen {
            framebuffer: fb,
            info,
            back_buffer: None,
            dirty: false,
            region_scratch: Vec::new(),
            vram: Some(mapping),
            pitch_pixels,
            clip: None,
            shadow: vec![0; info.width * info.height],
            pending_for_back_page: (0, 0, info.width as u32, info.height as u32),
        })
    }

    /// Get the global screen instance.
    /// Tries VRAM-backed mode first, falls back to legacy ioctl mode.
    pub fn get() -> Result<Screen> {
        match Self::new_vram() {
            Ok(s) => {
                eprintln!("screen: VRAM mmap mode");
                Ok(s)
            }
            Err(_) => {
                eprintln!("screen: legacy ioctl mode");
                let fb = Framebuffer::new();
                let info = fb.screen_info()?;
                let pitch_pixels = info.width;
                Ok(Screen {
                    framebuffer: fb,
                    info,
                    back_buffer: None,
                    dirty: false,
                    region_scratch: Vec::new(),
                    vram: None,
                    pitch_pixels,
                    clip: None,
                    shadow: Vec::new(),
                    pending_for_back_page: (0, 0, info.width as u32, info.height as u32),
                })
            }
        }
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

    /// Confine every subsequent draw to `rect`, or to the whole screen with
    /// `None`.
    ///
    /// This is what lets a compositor redraw one region instead of the screen:
    /// the drawing code stays the same and simply writes nothing outside the
    /// rectangle. A clip left set is a screen that stops updating, so it
    /// belongs in a narrow scope.
    pub fn set_clip(&mut self, rect: Option<(i32, i32, u32, u32)>) {
        self.clip = rect.map(|(x, y, w, h)| {
            let x0 = x.max(0) as usize;
            let y0 = y.max(0) as usize;
            let x1 = ((x as i64 + w as i64).max(0) as usize).min(self.info.width);
            let y1 = ((y as i64 + h as i64).max(0) as usize).min(self.info.height);
            (x0.min(x1), y0.min(y1), x1, y1)
        });
    }

    /// The bounds drawing is confined to, already intersected with the screen,
    /// as `(x0, y0, x1, y1)` with the far edges exclusive.
    ///
    /// Anything writing through [`Screen::pixels_mut`] has to apply this
    /// itself; the primitives on `Screen` already do.
    pub fn clip_bounds(&self) -> (usize, usize, usize, usize) {
        self.clip
            .unwrap_or((0, 0, self.info.width, self.info.height))
    }

    /// Ensure the back buffer is initialized (only used in non-VRAM mode).
    fn ensure_back_buffer(&mut self) -> Result<()> {
        if self.vram.is_none() && self.back_buffer.is_none() {
            let mut buffer = DrawRequest::new(self.width() as u64, self.height() as u64)?;
            buffer.clear();
            self.back_buffer = Some(buffer);
        }
        Ok(())
    }

    /// Returns a mutable pixel slice and row stride (in u32 units).
    /// In VRAM mode, returns the current back page slice and pitch_pixels.
    /// In Vec mode, returns the back buffer pixels and width.
    pub fn pixels_mut(&mut self) -> Option<(&mut [u32], usize)> {
        if self.vram.is_some() {
            // The shadow, not VRAM: see the field's comment. Its stride is the
            // screen width, which is not the VRAM pitch.
            return Some((&mut self.shadow, self.info.width));
        }
        let stride = self.pitch_pixels;
        if let Some(ref mut buf) = self.back_buffer {
            Some((&mut buf.pixels, stride))
        } else {
            None
        }
    }

    /// Copy a finished region from the shadow into VRAM.
    ///
    /// The only place VRAM is written, and it is a run of memcpys of pixels
    /// that are already complete, so the window in which the scanout holds a
    /// half-drawn frame is as short as the copy rather than as long as the
    /// compositing.
    fn publish(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if self.vram.is_none() {
            return;
        }
        // The display reads this buffer asynchronously, so the previous frame
        // may still be on its way out of it. This is the only place VRAM is
        // written, which makes it the one place the wait has to be: waiting
        // after the flip instead would let a compositing pass overwrite pixels
        // the host had not finished reading, and the overlap would tear.
        self.framebuffer.flip_wait();
        let (screen_w, screen_h) = (self.info.width, self.info.height);
        let x0 = (x as usize).min(screen_w);
        let y0 = (y as usize).min(screen_h);
        let x1 = ((x + w) as usize).min(screen_w);
        let y1 = ((y + h) as usize).min(screen_h);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let span = x1 - x0;
        let pitch = self.pitch_pixels;
        let shadow = &self.shadow;
        let Some(ref mut vram) = self.vram else {
            return;
        };
        let page = vram.back_page();
        for row in y0..y1 {
            let src = row * screen_w + x0;
            let dst = row * pitch + x0;
            if src + span <= shadow.len() && dst + span <= page.len() {
                page[dst..dst + span].copy_from_slice(&shadow[src..src + span]);
            }
        }
    }

    /// Render a line of interface text in the shell's outline face.
    ///
    /// Separate from `draw_text`, which is the bitmap path this replaces for
    /// chrome. Anything that is genuinely a character grid keeps the old one.
    pub fn draw_styled_text(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        style: crate::text::Style,
    ) -> Result<()> {
        self.ensure_back_buffer()?;
        let height = self.info.height as u32;
        let (cx0, cy0, cx1, cy1) = self.clip_bounds();
        let Some((pixels, stride)) = self.pixels_mut() else {
            return Ok(());
        };
        let mut surface = crate::surface::Surface::new(pixels, stride as u32, height);
        surface.clip = Some((cx0 as i32, cy0 as i32, cx1 as i32, cy1 as i32));
        crate::text::draw(&mut surface, x, y, text, style);
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

        let (cx0, cy0, cx1, cy1) = self.clip_bounds();

        if let Some((pixels, stride)) = self.pixels_mut() {
            let x0 = (x as usize).max(cx0);
            let y0 = (y as usize).max(cy0);
            let x1 = ((x + width) as usize).min(cx1);
            let y1 = ((y + height) as usize).min(cy1);
            if x1 <= x0 || y1 <= y0 {
                return Ok(());
            }
            let raw = color.raw();
            for py in y0..y1 {
                let row_start = py * stride + x0;
                pixels[row_start..row_start + (x1 - x0)].fill(raw);
            }
            self.dirty = true;
        }

        Ok(())
    }

    /// Fill the entire screen with a color
    pub fn fill(&mut self, color: Color) -> Result<()> {
        self.ensure_back_buffer()?;

        let screen_w = self.info.width;
        let screen_h = self.info.height;

        if let Some((pixels, stride)) = self.pixels_mut() {
            let raw = color.raw();
            for row in 0..screen_h {
                for col in 0..screen_w {
                    pixels[row * stride + col] = raw;
                }
            }
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
        if self.vram.is_some() {
            self.publish(0, 0, self.info.width as u32, self.info.height as u32);
            // Ensure all VRAM writes are visible before flipping pages.
            #[cfg(target_arch = "x86_64")]
            unsafe {
                core::arch::asm!("sfence", options(nostack, preserves_flags));
            }
            let offset = self.framebuffer.flip();
            if let Some(ref mut vram) = self.vram {
                vram.update_back_offset(offset);
            }
            self.dirty = false;
        } else {
            if self.dirty
                && let Some(ref buffer) = self.back_buffer
            {
                self.framebuffer.draw(buffer)?;
                self.dirty = false;
            }
            self.framebuffer.render();
            self.framebuffer.flip();
        }
        Ok(())
    }

    /// Render a sub-rectangle of the back buffer to the framebuffer.
    /// `x`, `y`, `w`, `h` are in screen coordinates. The region is clipped to
    /// the back buffer dimensions before sending.
    pub fn render_region(&mut self, x: u64, y: u64, w: u64, h: u64) -> Result<()> {
        let buf = match self.back_buffer {
            Some(ref b) => b,
            None => return Ok(()),
        };

        let screen_w = buf.width;
        let screen_h = buf.height;

        if x >= screen_w || y >= screen_h || w == 0 || h == 0 {
            return Ok(());
        }

        let actual_w = w.min(screen_w - x);
        let actual_h = h.min(screen_h - y);

        let pixel_count = (actual_w * actual_h) as usize;

        // Reuse scratch buffer; only grow, never shrink, to avoid repeated allocs.
        if self.region_scratch.len() < pixel_count {
            self.region_scratch.resize(pixel_count, 0);
        }

        // Pack rows from back buffer into contiguous scratch buffer.
        for row in 0..actual_h {
            let src_start = ((y + row) * screen_w + x) as usize;
            let dst_start = (row * actual_w) as usize;
            self.region_scratch[dst_start..dst_start + actual_w as usize]
                .copy_from_slice(&buf.pixels[src_start..src_start + actual_w as usize]);
        }

        self.framebuffer.draw_pixels(
            x,
            y,
            actual_w,
            actual_h,
            &self.region_scratch[..pixel_count],
        )?;

        self.dirty = false;
        Ok(())
    }

    /// Only call the present and flip syscalls, skip the back_buffer draw.
    /// Used by the WM after all dirty regions have been sent via render_region().
    pub fn render_present_only(&mut self) {
        self.framebuffer.render();
        let offset = self.framebuffer.flip();
        if let Some(ref mut vram) = self.vram {
            vram.update_back_offset(offset);
        }
    }

    /// Flip the display (full screen transfer).
    pub fn flip(&mut self) {
        let (w, h) = (self.info.width as u32, self.info.height as u32);
        self.publish(0, 0, w, h);
        // The page this one replaces has everything except the frame just
        // published, whatever it had before.
        self.pending_for_back_page = (0, 0, w, h);
        let offset = self.framebuffer.flip();
        if let Some(ref mut vram) = self.vram {
            vram.update_back_offset(offset);
        }
    }

    /// Publish several disjoint regions as one frame.
    ///
    /// `bounds` must cover every rect. On a page-flipping display the pages
    /// alternate, so the region the *other* page is missing has to go out with
    /// this frame too, and the bounding box is the honest answer there --
    /// splitting it would leave the other page stale.
    pub fn flip_rects(&mut self, rects: &[(u32, u32, u32, u32)], bounds: (u32, u32, u32, u32)) {
        if self.double_buffered() {
            let (x, y, w, h) = union_rect(self.pending_for_back_page, bounds);
            self.flip_rect(x, y, w, h);
            return;
        }
        let (bx, by, bw, bh) = bounds;
        self.publish(bx, by, bw, bh);
        self.pending_for_back_page = bounds;
        let offset = self.framebuffer.flip_rects(rects, bounds);
        if let Some(ref mut vram) = self.vram {
            vram.update_back_offset(offset);
        }
    }

    /// Flip only a dirty rectangle (partial transfer).
    pub fn flip_rect(&mut self, x: u32, y: u32, w: u32, h: u32) {
        // On a flipping display the page being written is a frame behind; see
        // `pending_for_back_page`. On a single-buffered one there is only ever
        // the one buffer and the rectangle is exactly what changed.
        let (px, py, pw, ph) = if self.double_buffered() {
            union_rect(self.pending_for_back_page, (x, y, w, h))
        } else {
            (x, y, w, h)
        };
        self.publish(px, py, pw, ph);
        self.pending_for_back_page = (x, y, w, h);
        let offset = self.framebuffer.flip_rect(px, py, pw, ph);
        if let Some(ref mut vram) = self.vram {
            vram.update_back_offset(offset);
        }
    }

    /// Whether the display alternates between two pages rather than scanning
    /// out one buffer.
    pub fn double_buffered(&self) -> bool {
        self.vram.as_ref().is_some_and(|vram| vram.double_buffered)
    }

    /// Set hardware cursor image. Returns true if supported.
    pub fn set_cursor(
        &self,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        pixels: &[u32],
    ) -> bool {
        self.framebuffer
            .set_cursor(width, height, hot_x, hot_y, pixels)
    }

    /// Move hardware cursor position.
    pub fn move_cursor(&self, x: u32, y: u32) {
        self.framebuffer.move_cursor(x, y);
    }

    /// Ask the display to keep its cursor plane on the pointer by itself,
    /// reporting whether it took it. See [`Framebuffer::track_pointer`].
    pub fn track_pointer(&self, enabled: bool) -> bool {
        self.framebuffer.track_pointer(enabled)
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
        self.ensure_back_buffer()?;

        if let Some(ref mut buffer) = self.back_buffer {
            buffer.blit_texture(texture, x, y)?;
            self.dirty = true;
        }

        Ok(())
    }

    /// Draw a texture with transparency (skip pixels with alpha=0).
    pub fn draw_texture_transparent(&mut self, texture: &Texture, x: u64, y: u64) -> Result<()> {
        self.ensure_back_buffer()?;

        let (cx0, cy0, cx1, cy1) = self.clip_bounds();

        if let Some((dst_pixels, stride)) = self.pixels_mut() {
            for src_y in 0..texture.height {
                for src_x in 0..texture.width {
                    let dst_x = x + src_x;
                    let dst_y = y + src_y;

                    if (dst_x as usize) < cx0
                        || (dst_x as usize) >= cx1
                        || (dst_y as usize) < cy0
                        || (dst_y as usize) >= cy1
                    {
                        continue;
                    }

                    let src_idx = (src_y * texture.width + src_x) as usize;
                    let src_pixel = texture.pixels[src_idx];

                    if src_pixel == 0 {
                        continue;
                    }

                    let dst_idx = (dst_y as usize) * stride + (dst_x as usize);
                    dst_pixels[dst_idx] = src_pixel;
                }
            }

            self.dirty = true;
        }

        Ok(())
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
        let height = self.info.height as u64;
        if let Some((pixels, stride)) = self.pixels_mut() {
            render_string_at(pixels, stride as u64, height, x, y, text, style)?;
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
        let height = self.info.height as u64;
        if let Some((pixels, stride)) = self.pixels_mut() {
            render_text_wrapped(pixels, stride as u64, height, x, y, text, style, wrap_width)?;
            self.dirty = true;
        }
        Ok(())
    }

    /// Draw a single character directly to the screen
    pub fn draw_char(&mut self, x: u64, y: u64, character: char, style: &TextStyle) -> Result<()> {
        let height = self.info.height as u64;
        if let Some((pixels, stride)) = self.pixels_mut() {
            render_character_at(pixels, stride as u64, height, x, y, character, style)?;
            self.dirty = true;
        }
        Ok(())
    }

    /// Set a single pixel in the back buffer.
    pub fn set_pixel(&mut self, x: u64, y: u64, color: Color) -> Result<()> {
        let screen_w = self.info.width as u64;
        let screen_h = self.info.height as u64;

        if x >= screen_w || y >= screen_h {
            return Err(GraphicsError::OutOfBounds);
        }

        let (cx0, cy0, cx1, cy1) = self.clip_bounds();
        if (x as usize) < cx0 || (x as usize) >= cx1 || (y as usize) < cy0 || (y as usize) >= cy1 {
            return Ok(());
        }

        if let Some((pixels, stride)) = self.pixels_mut() {
            pixels[(y as usize) * stride + (x as usize)] = color.raw();
            self.dirty = true;
        }

        Ok(())
    }

    /// Blit raw pixels directly to the back buffer without allocating intermediate structures.
    /// This is an optimized path for compositing window contents.
    pub fn blit_pixels_direct(
        &mut self,
        pixels: &[u32],
        src_width: u64,
        src_height: u64,
        dst_x: u64,
        dst_y: u64,
    ) -> Result<()> {
        self.ensure_back_buffer()?;

        let screen_width = self.info.width as u64;
        let screen_height = self.info.height as u64;

        let end_x = (dst_x + src_width).min(screen_width);
        let end_y = (dst_y + src_height).min(screen_height);

        if dst_x >= screen_width || dst_y >= screen_height {
            return Ok(());
        }

        if let Some((dst_pixels, stride)) = self.pixels_mut() {
            for src_y in 0..src_height {
                let screen_y = dst_y + src_y;
                if screen_y >= end_y {
                    break;
                }

                let src_row_start = (src_y * src_width) as usize;
                let dst_row_start = (screen_y as usize) * stride + (dst_x as usize);
                let copy_width = (end_x - dst_x) as usize;

                if src_row_start + copy_width <= pixels.len()
                    && dst_row_start + copy_width <= dst_pixels.len()
                {
                    dst_pixels[dst_row_start..dst_row_start + copy_width]
                        .copy_from_slice(&pixels[src_row_start..src_row_start + copy_width]);
                }
            }

            self.dirty = true;
        }

        Ok(())
    }

    /// Blit a clipped region of raw pixels to the back buffer.
    /// This is used for compositing windows that are partially off-screen.
    ///
    /// - `pixels`: Source pixel buffer
    /// - `src_width`, `src_height`: Full dimensions of the source buffer
    /// - `src_x`, `src_y`: Offset into the source buffer to start copying from
    /// - `dst_x`, `dst_y`: Destination position on screen
    /// - `copy_w`, `copy_h`: Dimensions of the region to copy
    pub fn blit_pixels_clipped(
        &mut self,
        pixels: &[u32],
        src_width: u64,
        src_height: u64,
        src_x: u64,
        src_y: u64,
        dst_x: u64,
        dst_y: u64,
        copy_w: u64,
        copy_h: u64,
    ) -> Result<()> {
        self.ensure_back_buffer()?;

        let screen_width = self.info.width as u64;
        let screen_height = self.info.height as u64;

        if src_x >= src_width || src_y >= src_height {
            return Ok(());
        }
        if dst_x >= screen_width || dst_y >= screen_height {
            return Ok(());
        }
        if copy_w == 0 || copy_h == 0 {
            return Ok(());
        }

        let actual_w = copy_w.min(src_width - src_x).min(screen_width - dst_x);
        let actual_h = copy_h.min(src_height - src_y).min(screen_height - dst_y);

        // Trim to the clip, moving the source origin by as much as the
        // destination moved so the pixels stay aligned with where they land.
        let (cx0, cy0, cx1, cy1) = self.clip_bounds();
        let skip_x = (cx0 as u64).saturating_sub(dst_x);
        let skip_y = (cy0 as u64).saturating_sub(dst_y);
        if skip_x >= actual_w || skip_y >= actual_h {
            return Ok(());
        }
        let (src_x, src_y) = (src_x + skip_x, src_y + skip_y);
        let (dst_x, dst_y) = (dst_x + skip_x, dst_y + skip_y);
        let actual_w = (actual_w - skip_x).min((cx1 as u64).saturating_sub(dst_x));
        let actual_h = (actual_h - skip_y).min((cy1 as u64).saturating_sub(dst_y));
        if actual_w == 0 || actual_h == 0 {
            return Ok(());
        }

        if let Some((dst_pixels, stride)) = self.pixels_mut() {
            for row in 0..actual_h {
                let src_row = src_y + row;
                let dst_row = dst_y + row;

                let src_start = (src_row * src_width + src_x) as usize;
                let dst_start = (dst_row as usize) * stride + (dst_x as usize);
                let width = actual_w as usize;

                if src_start + width <= pixels.len() && dst_start + width <= dst_pixels.len() {
                    dst_pixels[dst_start..dst_start + width]
                        .copy_from_slice(&pixels[src_start..src_start + width]);
                }
            }

            self.dirty = true;
        }

        Ok(())
    }
}

/// Get the global screen instance (convenience function)
pub fn screen() -> Result<Screen> {
    Screen::get()
}
