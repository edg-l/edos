use alloc::vec;
use alloc::vec::Vec;

pub mod api;
pub mod colors;

use crate::{
    boot::boot_info,
    graphics::api::{DrawRequest, REQUESTS, Request, Response, ScreenInfo},
    thread::{mailbox::Mailbox, scheduler::sched},
};

pub struct DoubleBuffer {
    back_buffer: Vec<u32>,
    width: usize,
    height: usize,
    pitch: usize,
    red_mask: u32,
    red_shift: u8,
    green_mask: u32,
    green_shift: u8,
    blue_mask: u32,
    blue_shift: u8,
}

impl DoubleBuffer {
    pub fn new() -> Self {
        let fb = &boot_info().framebuffer;
        let width = fb.width() as usize;
        let height = fb.height() as usize;
        let pitch = fb.pitch() as usize;

        // Cache color conversion masks and shifts
        let red_mask = (1u32 << fb.red_mask_size()) - 1;
        let red_shift = fb.red_mask_shift();
        let green_mask = (1u32 << fb.green_mask_size()) - 1;
        let green_shift = fb.green_mask_shift();
        let blue_mask = (1u32 << fb.blue_mask_size()) - 1;
        let blue_shift = fb.blue_mask_shift();

        Self {
            back_buffer: vec![0; (pitch * height) / 4],
            width,
            height,
            pitch,
            red_mask,
            red_shift,
            green_mask,
            green_shift,
            blue_mask,
            blue_shift,
        }
    }

    pub fn set_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x < self.width && y < self.height {
            let index = y * (self.pitch / 4) + x;
            self.back_buffer[index] = color;
        }
    }

    pub fn present(&self) {
        let fb = &boot_info().framebuffer;
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.back_buffer.as_ptr() as *const u8,
                fb.addr(),
                self.pitch * self.height,
            );
        }
    }

    /// Convert RGBA color (0xRRGGBBAA) to framebuffer's native format
    #[inline]
    fn convert_color(&self, rgba: u32) -> u32 {
        // Extract RGBA components from 0xRRGGBBAA
        let r = ((rgba >> 24) & 0xFF) as u8;
        let g = ((rgba >> 16) & 0xFF) as u8;
        let b = ((rgba >> 8) & 0xFF) as u8;
        let _a = (rgba & 0xFF) as u8; // Alpha not used for now

        // Convert to framebuffer format using cached masks
        let fb_r = ((r as u32 * self.red_mask) / 255) << self.red_shift;
        let fb_g = ((g as u32 * self.green_mask) / 255) << self.green_shift;
        let fb_b = ((b as u32 * self.blue_mask) / 255) << self.blue_shift;

        fb_r | fb_g | fb_b
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

        let pixels_per_row = self.pitch / 4; // Convert byte pitch to u32 pitch

        for y in start_y..end_y {
            for x in start_x..end_x {
                // Calculate position within the source rectangle
                let src_x = x - start_x;
                let src_y = y - start_y;

                // Index into the request's pixel data (width x height rectangle)
                let src_index = src_y * request.width as usize + src_x;

                // Calculate destination position in back buffer
                let dst_index = y * pixels_per_row + x;

                let rgba_color = request.pixels[src_index];
                let fb_color = self.convert_color(rgba_color);

                // Copy pixel (no bounds check needed since we clamped above)
                self.back_buffer[dst_index] = fb_color;
            }
        }
    }
}

pub fn render_thread() -> ! {
    let requests = REQUESTS.call_once(|| Mailbox::new(sched().current_id()));

    let mut display = DoubleBuffer::new();

    loop {
        while let Some(request) = requests.pop_request() {
            match &request.message {
                Request::ScreenInfo => {
                    let info = ScreenInfo {
                        height: display.height,
                        width: display.width,
                    };
                    request.answer(Response::ScreenInfo(info));
                }
                Request::Render => {
                    display.present();
                    request.answer(Response::Ok);
                }
                Request::Draw(draw_request) => {
                    display.draw(draw_request);
                    request.answer(Response::Ok);
                }
            }
        }
        sched().thread_yield();
    }
}
