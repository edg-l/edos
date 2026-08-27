use std::{fs::File, os::edos::io::FileExt};

use crate::surface::{Pixmap, Surface};
use edos_lib::mem::{self, MAP_PRIVATE, MAP_WRITE_COMBINING, PROT_READ, PROT_WRITE};
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

        let ptr = mem::mmap_physical(
            info.total_size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_WRITE_COMBINING,
            info.phys_addr,
        )
        .map_err(|_| GraphicsError::Fault)?
        .as_ptr()
        .cast::<u32>();

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
    #[expect(
        dead_code,
        reason = "the mapped byte length, kept beside the base pointer it was mapped with"
    )]
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

/// Render multi-line text with word wrapping
// Carries the destination buffer, its dimensions and the pen position as
// loose arguments because it rasterises straight into pixels; four of them
// collapse into one parameter the day it takes a `Surface`.
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
            // `DrawRequest::new` allocates zeroed pixels, so the buffer
            // starts black without a separate clear.
            self.back_buffer = Some(DrawRequest::new(self.width() as u64, self.height() as u64)?);
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

    /// A [`Surface`] over the back buffer, already carrying the screen's clip.
    ///
    /// This is the only rasteriser a `Screen` has: the compositor and the
    /// widgets fill rectangles, set text and blit through the same code, so a
    /// clipped or off-screen draw behaves identically wherever it comes from.
    /// `None` when there is no buffer to draw into.
    pub fn surface(&mut self) -> Option<Surface<'_>> {
        self.ensure_back_buffer().ok()?;
        let height = self.info.height as u32;
        let (cx0, cy0, cx1, cy1) = self.clip_bounds();
        let (pixels, stride) = self.pixels_mut()?;
        let mut surface = Surface::new(pixels, stride as u32, height);
        surface.clip = Some((cx0 as i32, cy0 as i32, cx1 as i32, cy1 as i32));
        Some(surface)
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
        if let Some(mut surface) = self.surface() {
            crate::text::draw(&mut surface, x, y, text, style);
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
        let raw = color.raw();
        if let Some(mut surface) = self.surface() {
            surface.rect(x as i32, y as i32, width as u32, height as u32, raw);
            self.dirty = true;
        }

        Ok(())
    }

    /// Fill the entire screen with a color
    pub fn fill(&mut self, color: Color) -> Result<()> {
        let (w, h) = (self.info.width as u32, self.info.height as u32);
        self.draw_rect(0, 0, w as u64, h as u64, color)
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

    /// Draw a texture with transparency (skip pixels with alpha=0).
    pub fn draw_texture_transparent(&mut self, texture: &Texture, x: u64, y: u64) -> Result<()> {
        let (tw, th) = (texture.width as u32, texture.height as u32);
        if let Some(mut surface) = self.surface() {
            // A zero pixel is the transparent one, so this is per pixel
            // rather than a row copy.
            for row in 0..th {
                for col in 0..tw {
                    let pixel = texture.pixels[(row * tw + col) as usize];
                    if pixel != 0 {
                        surface.rect(x as i32 + col as i32, y as i32 + row as i32, 1, 1, pixel);
                    }
                }
            }
            self.dirty = true;
        }

        Ok(())
    }

    /// Set a single pixel in the back buffer.
    pub fn set_pixel(&mut self, x: u64, y: u64, color: Color) -> Result<()> {
        if x >= self.info.width as u64 || y >= self.info.height as u64 {
            return Err(GraphicsError::OutOfBounds);
        }
        self.draw_rect(x, y, 1, 1, color)
    }

    /// Blit a clipped region of raw pixels to the back buffer.
    ///
    /// - `pixels`, `src_width`, `src_height`: the source buffer and its full
    ///   dimensions
    /// - `src_x`, `src_y`: where in the source the copy starts
    /// - `dst_x`, `dst_y`: where on screen it lands
    /// - `copy_w`, `copy_h`: how much to copy, before clipping
    #[expect(
        clippy::too_many_arguments,
        reason = "the compositor's own call shape: a source rectangle, a destination point and a size, each already a pair"
    )]
    // source rectangle, a destination point and a size, each already a pair.
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
        let src = Pixmap {
            pixels,
            width: src_width as u32,
            height: src_height as u32,
        };
        if let Some(mut surface) = self.surface() {
            surface.blit_region(
                &src,
                (src_x as u32, src_y as u32),
                (dst_x as i32, dst_y as i32),
                (copy_w as u32, copy_h as u32),
            );
            self.dirty = true;
        }
        Ok(())
    }
}

/// Get the global screen instance (convenience function)
pub fn screen() -> Result<Screen> {
    Screen::get()
}
