pub mod api;
pub mod colors;
pub mod framebuffer;

use crate::{
    boot::boot_info,
    graphics::{
        api::{DrawRequest, REQUESTS, Request, Response, ScreenInfo},
        framebuffer::FramebufferDevice,
    },
    thread::{mailbox::Mailbox, util::queue_spawn_kthread_named},
};

pub fn init() {
    queue_spawn_kthread_named("framebuffer", render_thread as u64);
}

pub struct DirectFramebuffer {
    width: usize,
    height: usize,
    pitch: usize,
    red_lut: [u32; 256],
    green_lut: [u32; 256],
    blue_lut: [u32; 256],
}

impl DirectFramebuffer {
    pub fn new() -> Self {
        let fb = &boot_info().framebuffer;
        let width = fb.width() as usize;
        let height = fb.height() as usize;
        let pitch = fb.pitch() as usize;

        let red_lut = Self::build_channel_lut(fb.red_mask_size(), fb.red_mask_shift());
        let green_lut = Self::build_channel_lut(fb.green_mask_size(), fb.green_mask_shift());
        let blue_lut = Self::build_channel_lut(fb.blue_mask_size(), fb.blue_mask_shift());

        Self {
            width,
            height,
            pitch,
            red_lut,
            green_lut,
            blue_lut,
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
        // Extract RGB components from 0x00RRGGBB
        let r = ((rgb >> 16) & 0xFF) as u8;
        let g = ((rgb >> 8) & 0xFF) as u8;
        let b = (rgb & 0xFF) as u8;

        // Convert to framebuffer format using cached lookup tables
        self.red_lut[r as usize] | self.green_lut[g as usize] | self.blue_lut[b as usize]
    }

    pub fn draw(&mut self, request: &DrawRequest) {
        // Clamp the drawing area to screen bounds
        let start_x = request.x.min(self.width as u64) as usize;
        let start_y = request.y.min(self.height as u64) as usize;

        let end_x = (request.x + request.width).min(self.width as u64) as usize;
        let end_y = (request.y + request.height).min(self.height as u64) as usize;

        // If completely outside bounds, return early
        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let fb = &boot_info().framebuffer;
        let pixels_per_row = self.pitch / 4; // Convert byte pitch to u32 pitch
        let src_width = request.width as usize;
        let src_offset_x = (start_x as u64 - request.x) as usize;
        let src_offset_y = (start_y as u64 - request.y) as usize;
        let row_len = end_x - start_x;

        // Pre-allocate temporary buffer for one row of converted pixels
        let mut converted_row = alloc::vec![0u32; row_len];

        let mut src_row = src_offset_y;
        for dst_y in start_y..end_y {
            let src_start_index = src_row * src_width + src_offset_x;
            let dst_start_index = dst_y * pixels_per_row + start_x;

            // Convert entire row from RGB to framebuffer format
            for i in 0..row_len {
                let rgb_color = request.pixels[src_start_index + i];
                converted_row[i] = self.convert_color(rgb_color);
            }

            // Bulk copy the entire converted row to framebuffer
            unsafe {
                let fb_ptr = fb.addr() as *mut u32;
                let dst_ptr = fb_ptr.add(dst_start_index);
                core::ptr::copy_nonoverlapping(converted_row.as_ptr(), dst_ptr, row_len);
            }

            src_row += 1;
        }
    }
}

pub extern "C" fn render_thread() -> ! {
    let mail = REQUESTS.call_once(Mailbox::new);

    FramebufferDevice::register();

    let mut display = DirectFramebuffer::new();

    loop {
        let mut request = mail.recv();
        match request.payload.take().unwrap() {
            Request::ScreenInfo => {
                let info = ScreenInfo {
                    height: display.height,
                    width: display.width,
                };
                Mailbox::reply(request, Response::ScreenInfo(info));
            }
            Request::Render => {
                // No-op since we write directly to framebuffer
                Mailbox::reply(request, Response::Ok);
            }
            Request::Draw(draw_request) => {
                display.draw(&draw_request);
                Mailbox::reply(request, Response::Ok);
            }
        }
    }
}
