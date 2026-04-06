extern crate alloc;
use alloc::vec::Vec;

pub mod colors;
pub mod framebuffer;

use spin::{Mutex, Once};

use crate::{
    boot::boot_info,
    drivers::vga::controller::{
        DISPI_INDEX_VIDEO_MEMORY_64K, DISPI_INDEX_VIRT_HEIGHT, DISPI_INDEX_Y_OFFSET, dispi_read,
        dispi_write,
    },
    println,
};

/// Global framebuffer, initialized once and accessed directly from ioctl handlers.
/// No render thread or Mailbox -- callers write to the framebuffer in their own
/// thread context, serialized by this Mutex.
pub static DISPLAY: Once<Mutex<DirectFramebuffer>> = Once::new();

pub fn init() {
    DISPLAY.call_once(|| {
        let display = DirectFramebuffer::new();
        framebuffer::FramebufferDevice::register();
        Mutex::new(display)
    });
}

/// Screen dimensions, returned by the SCREEN_INFO ioctl.
#[derive(Debug, Clone, Copy)]
pub struct ScreenInfo {
    pub width: usize,
    pub height: usize,
}

pub struct DirectFramebuffer {
    width: usize,
    height: usize,
    pitch: usize,
    /// Base virtual address of the framebuffer (page 0).
    fb_base: *mut u32,
    /// True when the framebuffer pixel format is 32-bit 0x00RRGGBB
    /// (R=8 bits at shift 16, G=8 bits at shift 8, B=8 bits at shift 0,
    /// pitch aligned to 4 bytes). In this case we can memcpy rows directly
    /// without per-pixel color conversion.
    is_identity: bool,
    red_lut: [u32; 256],
    green_lut: [u32; 256],
    blue_lut: [u32; 256],
    converted_row_buffer: Vec<u32>,
    double_buffered: bool,
    /// Y offset in pixels of the back page (the page we draw into).
    back_page_y_offset: usize,
}

unsafe impl Send for DirectFramebuffer {}

impl DirectFramebuffer {
    pub fn new() -> Self {
        let fb = &boot_info().framebuffer;
        let width = fb.width() as usize;
        let height = fb.height() as usize;
        let pitch = fb.pitch() as usize;
        let fb_base = fb.addr() as *mut u32;

        let red_lut = Self::build_channel_lut(fb.red_mask_size(), fb.red_mask_shift());
        let green_lut = Self::build_channel_lut(fb.green_mask_size(), fb.green_mask_shift());
        let blue_lut = Self::build_channel_lut(fb.blue_mask_size(), fb.blue_mask_shift());

        let is_identity = fb.bpp() == 32
            && fb.red_mask_size() == 8
            && fb.red_mask_shift() == 16
            && fb.green_mask_size() == 8
            && fb.green_mask_shift() == 8
            && fb.blue_mask_size() == 8
            && fb.blue_mask_shift() == 0
            && pitch % 4 == 0;

        println!(
            "Framebuffer: {}x{} bpp={} identity={} (R={}@{} G={}@{} B={}@{})",
            width,
            height,
            fb.bpp(),
            is_identity,
            fb.red_mask_size(),
            fb.red_mask_shift(),
            fb.green_mask_size(),
            fb.green_mask_shift(),
            fb.blue_mask_size(),
            fb.blue_mask_shift(),
        );

        let two_page_bytes = 2 * height * pitch;
        let vram_64k = dispi_read(DISPI_INDEX_VIDEO_MEMORY_64K) as usize;
        let vram_bytes = vram_64k * 64 * 1024;
        let double_buffered = vram_bytes >= two_page_bytes && pitch % 4 == 0;

        if double_buffered {
            dispi_write(DISPI_INDEX_VIRT_HEIGHT, (2 * height) as u16);
            dispi_write(DISPI_INDEX_Y_OFFSET, 0);
            println!(
                "Framebuffer: double buffering enabled (VRAM={}KB, need={}KB)",
                vram_bytes / 1024,
                two_page_bytes / 1024
            );
        } else {
            println!(
                "Framebuffer: double buffering disabled (VRAM={}KB, need={}KB)",
                vram_bytes / 1024,
                two_page_bytes / 1024
            );
        }

        Self {
            width,
            height,
            pitch,
            fb_base,
            is_identity,
            red_lut,
            green_lut,
            blue_lut,
            // Pre-allocate to screen width so draw() never allocates under the lock.
            converted_row_buffer: alloc::vec![0u32; width],
            double_buffered,
            back_page_y_offset: if double_buffered { height } else { 0 },
        }
    }

    pub fn screen_info(&self) -> ScreenInfo {
        ScreenInfo {
            width: self.width,
            height: self.height,
        }
    }

    fn build_channel_lut(mask_size: u8, shift: u8) -> [u32; 256] {
        let mask = if mask_size == 0 {
            0
        } else {
            (1u32 << mask_size.min(31)) - 1
        };

        core::array::from_fn(|value| {
            if mask == 0 {
                0
            } else {
                ((value as u32 * mask) / 255) << shift
            }
        })
    }

    /// Convert RGB color (0x00RRGGBB) to framebuffer's native format
    #[inline]
    fn convert_color(&self, rgb: u32) -> u32 {
        let r = ((rgb >> 16) & 0xFF) as u8;
        let g = ((rgb >> 8) & 0xFF) as u8;
        let b = (rgb & 0xFF) as u8;
        self.red_lut[r as usize] | self.green_lut[g as usize] | self.blue_lut[b as usize]
    }

    /// Draw pixels from `src` at position (x, y) with dimensions (src_width x src_height).
    /// Clips to screen bounds. `src` must contain at least `src_width * src_height` pixels.
    pub fn draw(&mut self, src: &[u32], x: u64, y: u64, src_width: usize, src_height: usize) {
        let start_x = x.min(self.width as u64) as usize;
        let start_y = y.min(self.height as u64) as usize;
        let end_x = (x + src_width as u64).min(self.width as u64) as usize;
        let end_y = (y + src_height as u64).min(self.height as u64) as usize;

        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let pixels_per_row = self.pitch / 4;
        let src_offset_x = start_x - x as usize;
        let src_offset_y = start_y - y as usize;
        let row_len = end_x - start_x;
        let back_y = self.back_page_y_offset;

        // Bounds-check: verify the last row we'll read is within src
        let last_src_row = src_offset_y + (end_y - start_y) - 1;
        let last_src_index = last_src_row * src_width + src_offset_x + row_len;
        if last_src_index > src.len() {
            return;
        }

        if self.is_identity {
            // Fast path: memcpy rows directly to framebuffer
            let mut src_row = src_offset_y;
            for dst_y in start_y..end_y {
                let src_start = src_row * src_width + src_offset_x;
                let dst_start = (back_y + dst_y) * pixels_per_row + start_x;

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src[src_start..].as_ptr(),
                        self.fb_base.add(dst_start),
                        row_len,
                    );
                }

                src_row += 1;
            }
        } else {
            // Slow path: per-pixel LUT color conversion.
            // converted_row_buffer is pre-allocated to screen width in new().
            let red_lut = &self.red_lut;
            let green_lut = &self.green_lut;
            let blue_lut = &self.blue_lut;
            let conv = &mut self.converted_row_buffer[..row_len];

            let mut src_row = src_offset_y;
            for dst_y in start_y..end_y {
                let src_start = src_row * src_width + src_offset_x;
                let dst_start = (back_y + dst_y) * pixels_per_row + start_x;

                for i in 0..row_len {
                    let rgb = src[src_start + i];
                    let r = ((rgb >> 16) & 0xFF) as usize;
                    let g = ((rgb >> 8) & 0xFF) as usize;
                    let b = (rgb & 0xFF) as usize;
                    conv[i] = red_lut[r] | green_lut[g] | blue_lut[b];
                }

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        conv.as_ptr(),
                        self.fb_base.add(dst_start),
                        row_len,
                    );
                }

                src_row += 1;
            }
        }
    }

    /// Fill a rectangle with a solid color.
    pub fn draw_rect(&mut self, x: u64, y: u64, width: u64, height: u64, color: u32) {
        if width == 0 || height == 0 {
            return;
        }

        let start_x = x.min(self.width as u64) as usize;
        let start_y = y.min(self.height as u64) as usize;
        let end_x = (x + width).min(self.width as u64) as usize;
        let end_y = (y + height).min(self.height as u64) as usize;

        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let pixels_per_row = self.pitch / 4;
        let row_len = end_x - start_x;
        let back_y = self.back_page_y_offset;
        let native_color = if self.is_identity {
            color
        } else {
            self.convert_color(color)
        };

        // Fill one row in the pre-allocated buffer, then memcpy to each scanline.
        let row_buf = &mut self.converted_row_buffer[..row_len];
        row_buf.fill(native_color);

        for dst_y in start_y..end_y {
            let dst_start = (back_y + dst_y) * pixels_per_row + start_x;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    row_buf.as_ptr(),
                    self.fb_base.add(dst_start),
                    row_len,
                );
            }
        }
    }

    /// Flip the display to show the back page. When double buffering is active
    /// this writes the current `front_y` to the Bochs VBE Y_OFFSET register,
    /// making the just-drawn page visible, then swaps front and back.
    pub fn flip(&mut self) {
        if !self.double_buffered {
            return;
        }
        // Show the back page (the one we just drew to).
        let show_y = self.back_page_y_offset as u16;
        dispi_write(DISPI_INDEX_Y_OFFSET, show_y);
        // The old front page becomes the new back page.
        if self.back_page_y_offset == 0 {
            self.back_page_y_offset = self.height;
        } else {
            self.back_page_y_offset = 0;
        }
    }
}
