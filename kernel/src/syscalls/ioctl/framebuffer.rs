use alloc::{boxed::Box, vec::Vec};

use crate::{
    fs::api as fs_api,
    graphics::framebuffer::{
        FB_IOCTL_DRAW, FB_IOCTL_DRAW_RECT, FB_IOCTL_RENDER, FB_IOCTL_SCREEN_INFO, FramebufferDraw,
        FramebufferDrawCommand, FramebufferInfo, FramebufferRect,
    },
    syscalls::Errno,
    thread::pipe::FsFile,
};
use x86_64::instructions::interrupts;

/// Attempt to handle framebuffer-specific ioctl requests. Returns `None` if the
/// request is not handled here so the caller can fall back to the generic
/// filesystem handler.
pub fn try_handle(file: &FsFile, request: u64, arg: u64) -> Option<Result<u64, Errno>> {
    match request {
        FB_IOCTL_DRAW_RECT => Some(handle_draw_rect(file, arg)),
        FB_IOCTL_DRAW => Some(handle_draw(file, arg)),
        FB_IOCTL_SCREEN_INFO => Some(handle_screen_info(file, arg)),
        FB_IOCTL_RENDER => Some(handle_render(file)),
        _ => None,
    }
}

fn handle_draw_rect(file: &FsFile, arg: u64) -> Result<u64, Errno> {
    if arg == 0 {
        return Err(Errno::EFAULT);
    }

    let rect_ptr = arg as *const FramebufferRect;
    if rect_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let rect = unsafe { *rect_ptr };
    let mut rect_box = Box::new(rect);

    interrupts::enable();
    fs_api::ioctl(
        &file.path,
        FB_IOCTL_DRAW_RECT,
        rect_box.as_mut() as *mut FramebufferRect as u64,
    )
    .map_err(Errno::from)
}

fn handle_draw(file: &FsFile, arg: u64) -> Result<u64, Errno> {
    if arg == 0 {
        return Err(Errno::EFAULT);
    }

    let draw_ptr = arg as *const FramebufferDraw;
    if draw_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let draw = unsafe { *draw_ptr };

    if draw.width == 0 || draw.height == 0 {
        return Err(Errno::EINVAL);
    }

    let Some(total_pixels) = draw.width.checked_mul(draw.height) else {
        return Err(Errno::EINVAL);
    };

    let pixel_count = usize::try_from(total_pixels).map_err(|_| Errno::EINVAL)?;

    if draw.pixels.is_null() {
        return Err(Errno::EFAULT);
    }

    let user_pixels = unsafe { core::slice::from_raw_parts(draw.pixels, pixel_count) };
    let mut pixels = Vec::new();
    if pixels.try_reserve_exact(pixel_count).is_err() {
        return Err(Errno::ENOMEM);
    }
    pixels.extend_from_slice(user_pixels);

    let command = FramebufferDrawCommand {
        x: draw.x,
        y: draw.y,
        width: draw.width,
        height: draw.height,
        pixels: pixels.into_boxed_slice(),
    };

    let command_ptr = Box::into_raw(Box::new(command));

    interrupts::enable();
    match fs_api::ioctl(&file.path, FB_IOCTL_DRAW, command_ptr as u64) {
        Ok(res) => Ok(res),
        Err(err) => {
            unsafe {
                drop(Box::from_raw(command_ptr));
            }
            Err(Errno::from(err))
        }
    }
}

fn handle_screen_info(file: &FsFile, arg: u64) -> Result<u64, Errno> {
    if arg == 0 {
        return Err(Errno::EFAULT);
    }

    let info_ptr = arg as *mut FramebufferInfo;
    if info_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let mut info_box = Box::new(FramebufferInfo::default());

    interrupts::enable();
    match fs_api::ioctl(
        &file.path,
        FB_IOCTL_SCREEN_INFO,
        info_box.as_mut() as *mut FramebufferInfo as u64,
    ) {
        Ok(res) => {
            unsafe {
                core::ptr::write(info_ptr, *info_box);
            }
            Ok(res)
        }
        Err(err) => Err(Errno::from(err)),
    }
}

fn handle_render(file: &FsFile) -> Result<u64, Errno> {
    interrupts::enable();
    fs_api::ioctl(&file.path, FB_IOCTL_RENDER, 0).map_err(Errno::from)
}
