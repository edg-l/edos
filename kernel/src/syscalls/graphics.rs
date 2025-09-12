use alloc::vec::Vec;

use crate::{
    graphics::api::{DrawRequest, ScreenInfo, draw, draw_rect, render, screen_info},
    syscalls::Errno,
    thread::scheduler::sched,
};

pub fn sys_draw_rect(x: u64, y: u64, width: u64, height: u64, color: u32) -> u64 {
    let sched = sched();
    sched.current_thread_clear_errno();

    // Basic validation
    if width == 0 || height == 0 {
        sched.current_thread_set_errno(Errno::EINVAL);
        return !0u64; // -1
    }

    x86_64::instructions::interrupts::enable();
    draw_rect(x, y, width, height, color);
    0 // success
}

pub fn sys_render() -> u64 {
    let sched = sched();
    sched.current_thread_clear_errno();
    x86_64::instructions::interrupts::enable();
    render();
    0
}

pub fn sys_screen_info(info_ptr: *mut ScreenInfo) -> u64 {
    {
        let sched = sched();
        sched.current_thread_clear_errno();

        if info_ptr.is_null() {
            sched.current_thread_set_errno(Errno::EFAULT);
            return !0u64; // -1
        }
    }

    x86_64::instructions::interrupts::enable();
    let info = screen_info();
    unsafe { *info_ptr = info };
    0
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct DrawRequestInput {
    pub pixels: *const u32,
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
}

pub fn sys_draw(request_ptr: *const DrawRequestInput) -> u64 {
    let sched = sched();
    sched.current_thread_clear_errno();

    if request_ptr.is_null() {
        sched.current_thread_set_errno(Errno::EFAULT);
        return !0u64;
    }

    // Copy the DrawRequest from user space
    let request = unsafe { &*request_ptr };

    // Basic validation
    if request.width == 0 || request.height == 0 {
        sched.current_thread_set_errno(Errno::EINVAL);
        return !0u64;
    }

    let mut pixels = Vec::new();
    let slice = unsafe {
        core::slice::from_raw_parts(request.pixels, (request.width * request.height) as usize)
    };
    pixels.extend_from_slice(slice);
    let kernel_request = DrawRequest {
        pixels: pixels.into_boxed_slice(),
        height: request.height,
        width: request.width,
        x: request.x,
        y: request.y,
    };

    x86_64::instructions::interrupts::enable();
    draw(kernel_request);
    0
}
