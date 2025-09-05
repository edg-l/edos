use crate::{
    graphics::api::{DrawRequest, ScreenInfo, draw, draw_rect, render, screen_info},
    syscalls::Errno,
    thread::scheduler::sched,
};

pub fn sys_draw_rect(x: u64, y: u64, width: u64, height: u64, color: u32) -> u64 {
    let sched = sched();
    let thread = sched.current_thread_mut();
    thread.errno = Errno::Clear;

    // Basic validation
    if width == 0 || height == 0 {
        thread.errno = Errno::EINVAL;
        return !0u64; // -1
    }

    x86_64::instructions::interrupts::enable();
    draw_rect(x, y, width, height, color);
    0 // success
}

pub fn sys_render() -> u64 {
    let sched = sched();
    let thread = sched.current_thread_mut();
    thread.errno = Errno::Clear;

    x86_64::instructions::interrupts::enable();
    render();
    0
}

pub fn sys_screen_info(info_ptr: *mut ScreenInfo) -> u64 {
    {
        let sched = sched();
        let thread = sched.current_thread_mut();
        thread.errno = Errno::Clear;

        if info_ptr.is_null() {
            thread.errno = Errno::EFAULT;
            return !0u64; // -1
        }
    }

    x86_64::instructions::interrupts::enable();
    let info = screen_info();
    unsafe { *info_ptr = info };
    0
}

pub fn sys_draw(request_ptr: *const DrawRequest) -> u64 {
    let sched = sched();
    let thread = sched.current_thread_mut();
    thread.errno = Errno::Clear;

    if request_ptr.is_null() {
        thread.errno = Errno::EFAULT;
        return !0u64;
    }

    // Copy the DrawRequest from user space
    let request = unsafe { &*request_ptr };

    // Basic validation
    if request.width == 0 || request.height == 0 {
        thread.errno = Errno::EINVAL;
        return !0u64;
    }

    if request.pixels.len() != (request.width * request.height) as usize {
        thread.errno = Errno::EINVAL;
        return !0u64;
    }

    x86_64::instructions::interrupts::enable();
    draw(request.clone());
    0
}
