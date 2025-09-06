use alloc::vec::Vec;
use thiserror::Error;

use crate::{
    Errno, SYS_DRAW, SYS_DRAW_RECT, SYS_RENDER, SYS_SCREEN_INFO, math::isqrt, sys_errno, syscall,
};

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
}

impl From<Errno> for GraphicsError {
    fn from(errno: Errno) -> Self {
        match errno {
            Errno::EINVAL => GraphicsError::InvalidInput,
            Errno::ENOMEM => GraphicsError::OutOfMemory,
            Errno::EFAULT => GraphicsError::Fault,
            Errno::UNKNOWN => GraphicsError::Unknown,
            Errno::Clear => GraphicsError::Unknown,
        }
    }
}

pub type GraphicsResult<T> = Result<T, GraphicsError>;

/// Type-safe color representation (ARGB format)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(u32);

impl Color {
    /// Create a new color from ARGB components
    pub const fn from_argb(a: u8, r: u8, g: u8, b: u8) -> Self {
        Self(((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }

    /// Create a new color from RGB components (alpha = 255)
    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self::from_argb(255, r, g, b)
    }

    /// Get the raw u32 value
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Extract alpha component
    pub const fn alpha(self) -> u8 {
        ((self.0 >> 24) & 0xFF) as u8
    }

    /// Extract red component
    pub const fn red(self) -> u8 {
        ((self.0 >> 16) & 0xFF) as u8
    }

    /// Extract green component
    pub const fn green(self) -> u8 {
        ((self.0 >> 8) & 0xFF) as u8
    }

    /// Extract blue component
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
    pub const TRANSPARENT: Color = Color::from_argb(0, 0, 0, 0);
}

impl From<u32> for Color {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<Color> for u32 {
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
    pub const fn new(x: u64, y: u64, width: u64, height: u64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
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
    pub fn new(width: u64, height: u64) -> GraphicsResult<Self> {
        if width == 0 || height == 0 {
            return Err(GraphicsError::InvalidInput);
        }

        let pixel_count = width
            .checked_mul(height)
            .ok_or(GraphicsError::InvalidInput)? as usize;

        let pixels = alloc::vec![0; pixel_count];

        Ok(Self {
            pixels,
            width,
            height,
        })
    }

    /// Create a texture from existing pixel buffer
    pub fn from_buffer(width: u64, height: u64, pixels: Vec<u32>) -> GraphicsResult<Self> {
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
    pub fn set_pixel(&mut self, x: u64, y: u64, color: Color) -> GraphicsResult<()> {
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
    pub fn get_pixel(&self, x: u64, y: u64) -> GraphicsResult<Color> {
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
    ) -> GraphicsResult<()> {
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

    /// Clear the texture (set all pixels to transparent)
    pub fn clear(&mut self) {
        self.fill(Color::TRANSPARENT);
    }
}

/// Blending modes for texture operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    /// Replace destination with source
    Replace,
    /// Alpha blend source over destination
    AlphaBlend,
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
            BlendMode::AlphaBlend => {
                let src_alpha = src.alpha() as u32;
                let inv_alpha = 255 - src_alpha;

                let r = (src.red() as u32 * src_alpha + dst.red() as u32 * inv_alpha) / 255;
                let g = (src.green() as u32 * src_alpha + dst.green() as u32 * inv_alpha) / 255;
                let b = (src.blue() as u32 * src_alpha + dst.blue() as u32 * inv_alpha) / 255;
                let a = src_alpha.max(dst.alpha() as u32);

                Color::from_argb(a as u8, r as u8, g as u8, b as u8)
            }
            BlendMode::Add => {
                let r = (src.red() as u32 + dst.red() as u32).min(255);
                let g = (src.green() as u32 + dst.green() as u32).min(255);
                let b = (src.blue() as u32 + dst.blue() as u32).min(255);
                let a = src.alpha().max(dst.alpha());

                Color::from_argb(a, r as u8, g as u8, b as u8)
            }
            BlendMode::Multiply => {
                let r = (src.red() as u32 * dst.red() as u32) / 255;
                let g = (src.green() as u32 * dst.green() as u32) / 255;
                let b = (src.blue() as u32 * dst.blue() as u32) / 255;
                let a = src.alpha().max(dst.alpha());

                Color::from_argb(a, r as u8, g as u8, b as u8)
            }
        }
    }
}

/// Draw a rectangle directly to the screen
pub fn draw_rect(x: u64, y: u64, width: u64, height: u64, color: Color) -> GraphicsResult<()> {
    let result =
        unsafe { syscall::syscall5(SYS_DRAW_RECT, x, y, width, height, color.raw() as u64) };
    if result == !0u64 {
        Err(GraphicsError::from(sys_errno()))
    } else {
        Ok(())
    }
}

/// Render all pending draw operations to the screen
pub fn render() -> GraphicsResult<()> {
    let result = unsafe { syscall::syscall0(SYS_RENDER) };
    if result == !0u64 {
        Err(GraphicsError::from(sys_errno()))
    } else {
        Ok(())
    }
}

/// Get screen information
pub fn screen_info() -> GraphicsResult<ScreenInfo> {
    let mut info = ScreenInfo {
        width: 0,
        height: 0,
    };
    let result = unsafe { syscall::syscall1(SYS_SCREEN_INFO, &mut info as *mut _ as u64) };

    if result == !0u64 {
        Err(GraphicsError::from(sys_errno()))
    } else {
        Ok(info)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ScreenInfo {
    pub width: usize,
    pub height: usize,
}

#[repr(C)]
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
    pub fn new(width: u64, height: u64) -> GraphicsResult<Self> {
        if width == 0 || height == 0 {
            return Err(GraphicsError::InvalidInput);
        }

        let pixel_count = width
            .checked_mul(height)
            .ok_or(GraphicsError::InvalidInput)? as usize;

        let pixels = alloc::vec![0; pixel_count];

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
    pub fn set_pixel(&mut self, x: u64, y: u64, color: Color) -> GraphicsResult<()> {
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
    pub fn get_pixel(&self, x: u64, y: u64) -> GraphicsResult<Color> {
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
    ) -> GraphicsResult<()> {
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

    /// Clear the DrawRequest (set all pixels to transparent)
    pub fn clear(&mut self) {
        self.fill(Color::TRANSPARENT);
    }

    /// Draw a line from (x1, y1) to (x2, y2) using Bresenham's algorithm
    pub fn draw_line(
        &mut self,
        x1: u64,
        y1: u64,
        x2: u64,
        y2: u64,
        color: Color,
    ) -> GraphicsResult<()> {
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
    pub fn draw_circle(
        &mut self,
        cx: u64,
        cy: u64,
        radius: u64,
        color: Color,
    ) -> GraphicsResult<()> {
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
    pub fn fill_circle(
        &mut self,
        cx: u64,
        cy: u64,
        radius: u64,
        color: Color,
    ) -> GraphicsResult<()> {
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

            let dx = isqrt(under_sqrt as u64) as i64;

            let start = (cx - dx).max(0) as u64;
            let end = (cx + dx).min(self.width as i64 - 1) as u64;

            if start <= end {
                self.fill_rect(start, y as u64, end - start + 1, 1, color)?;
            }
        }
        Ok(())
    }

    /// Blit a texture to this DrawRequest at the specified position
    pub fn blit_texture(
        &mut self,
        texture: &Texture,
        dst_x: u64,
        dst_y: u64,
    ) -> GraphicsResult<()> {
        self.blit_texture_with_blend(texture, None, dst_x, dst_y, BlendMode::Replace)
    }

    /// Blit a texture region to this DrawRequest at the specified position
    pub fn blit_texture_region(
        &mut self,
        texture: &Texture,
        src_rect: Rect,
        dst_x: u64,
        dst_y: u64,
    ) -> GraphicsResult<()> {
        self.blit_texture_with_blend(texture, Some(src_rect), dst_x, dst_y, BlendMode::Replace)
    }

    /// Blit a texture with alpha blending
    pub fn blit_texture_alpha(
        &mut self,
        texture: &Texture,
        dst_x: u64,
        dst_y: u64,
    ) -> GraphicsResult<()> {
        self.blit_texture_with_blend(texture, None, dst_x, dst_y, BlendMode::AlphaBlend)
    }

    /// Blit a texture with custom blending mode
    pub fn blit_texture_with_blend(
        &mut self,
        texture: &Texture,
        src_rect: Option<Rect>,
        dst_x: u64,
        dst_y: u64,
        blend_mode: BlendMode,
    ) -> GraphicsResult<()> {
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
    ) -> GraphicsResult<()> {
        let texture = Texture::from_buffer(width, height, pixels.to_vec())?;
        self.blit_texture(&texture, dst_x, dst_y)
    }

    /// Blit with scaling (simple nearest neighbor)
    pub fn blit_texture_scaled(
        &mut self,
        texture: &Texture,
        src_rect: Option<Rect>,
        dst_rect: Rect,
    ) -> GraphicsResult<()> {
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

    /// Draw this request to the screen
    pub fn draw(&self) -> GraphicsResult<()> {
        let result = unsafe { syscall::syscall1(SYS_DRAW, self as *const _ as u64) };
        if result == !0u64 {
            Err(GraphicsError::from(sys_errno()))
        } else {
            Ok(())
        }
    }
}

/// Screen management struct providing convenient graphics operations
pub struct Screen {
    info: ScreenInfo,
}

impl Screen {
    /// Get the global screen instance
    pub fn get() -> GraphicsResult<Self> {
        let info = screen_info()?;
        Ok(Self { info })
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

    /// Draw a rectangle on the screen
    pub fn draw_rect(
        &self,
        x: u64,
        y: u64,
        width: u64,
        height: u64,
        color: Color,
    ) -> GraphicsResult<()> {
        draw_rect(x, y, width, height, color)
    }

    /// Fill the entire screen with a color
    pub fn fill(&self, color: Color) -> GraphicsResult<()> {
        self.draw_rect(0, 0, self.width() as u64, self.height() as u64, color)
    }

    /// Clear the screen (fill with black)
    pub fn clear(&self) -> GraphicsResult<()> {
        self.fill(Color::BLACK)
    }

    /// Render all pending operations
    pub fn render(&self) -> GraphicsResult<()> {
        render()
    }

    /// Create a new DrawRequest that fits entirely on screen
    pub fn create_draw_request(&self, width: u64, height: u64) -> GraphicsResult<DrawRequest> {
        if width > self.width() as u64 || height > self.height() as u64 {
            return Err(GraphicsError::OutOfBounds);
        }
        DrawRequest::new(width, height)
    }

    /// Draw a texture directly to the screen at the specified position
    pub fn draw_texture(&self, texture: &Texture, x: u64, y: u64) -> GraphicsResult<()> {
        let mut draw_req = DrawRequest::new(texture.width, texture.height)?;
        draw_req.blit_texture(texture, 0, 0)?;
        draw_req = draw_req.with_position(x, y);
        draw_req.draw()
    }

    /// Draw a texture region directly to the screen
    pub fn draw_texture_region(
        &self,
        texture: &Texture,
        src_rect: Rect,
        x: u64,
        y: u64,
    ) -> GraphicsResult<()> {
        let mut draw_req = DrawRequest::new(src_rect.width, src_rect.height)?;
        draw_req.blit_texture_region(texture, src_rect, 0, 0)?;
        draw_req = draw_req.with_position(x, y);
        draw_req.draw()
    }

    /// Draw a texture with alpha blending directly to the screen
    pub fn draw_texture_alpha(&self, texture: &Texture, x: u64, y: u64) -> GraphicsResult<()> {
        let mut draw_req = DrawRequest::new(texture.width, texture.height)?;
        draw_req.blit_texture_alpha(texture, 0, 0)?;
        draw_req = draw_req.with_position(x, y);
        draw_req.draw()
    }

    /// Draw a texture with scaling directly to the screen
    pub fn draw_texture_scaled(&self, texture: &Texture, dst_rect: Rect) -> GraphicsResult<()> {
        let mut draw_req = DrawRequest::new(dst_rect.width, dst_rect.height)?;
        draw_req.blit_texture_scaled(
            texture,
            None,
            Rect::new(0, 0, dst_rect.width, dst_rect.height),
        )?;
        draw_req = draw_req.with_position(dst_rect.x, dst_rect.y);
        draw_req.draw()
    }

    /// Create a texture from the screen contents (screenshot)
    pub fn capture_region(&self, region: Rect) -> GraphicsResult<Texture> {
        // This would require a new syscall to read from framebuffer
        // For now, return an error as this functionality isn't implemented
        Err(GraphicsError::Unknown)
    }
}

/// Get the global screen instance (convenience function)
pub fn screen() -> GraphicsResult<Screen> {
    Screen::get()
}
