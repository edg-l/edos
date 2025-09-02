use alloc::vec;
use alloc::vec::Vec;
use spin::Once;
use x86_64::instructions::hlt;

pub mod api;

use crate::{
    boot::boot_info, graphics::api::{Request, Response, ScreenInfo, REQUESTS}, thread::{mailbox::Mailbox, scheduler::sched}
};

pub struct DoubleBuffer {
    back_buffer: Vec<u32>,
    width: usize,
    height: usize,
    pitch: usize,
}

impl DoubleBuffer {
    pub fn new() -> Self {
        let fb = &boot_info().framebuffer;
        let width = fb.width() as usize;
        let height = fb.height() as usize;
        let pitch = fb.pitch() as usize;

        Self {
            back_buffer: vec![0; (pitch * height) / 4], // /4 because pitch is in bytes, we store u32
            width,
            height,
            pitch,
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
}





pub fn render_thread() -> ! {
    let requests = REQUESTS.call_once(|| Mailbox::new(sched().current_id()));

    let mut display = DoubleBuffer::new();

    for i in 0..100_u64 {
        display.set_pixel(i as usize, i as usize, 0xFFFFFFFF);
    }

    loop {
        while let Some(request) = requests.pop_request() {
            match request.message {
                Request::ScreenInfo => {
                    let info = ScreenInfo {
                        height: display.height,
                        width: display.width,
                    };
                    request.answer(Response::ScreenInfo(info));
                }
                Request::Render => {
                    display.present();
                    request.answer(Response::Rendered);
                }
            }
        }
        hlt();
    }
}
