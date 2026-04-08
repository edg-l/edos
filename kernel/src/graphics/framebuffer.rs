use alloc::sync::Arc;

use crate::{
    fs::{DevFsDevice, DevFsError, register_device_str},
    graphics::DISPLAY,
};

pub const FB_IOCTL_DRAW_RECT: u64 = 0x4642_0001;
pub const FB_IOCTL_RENDER: u64 = 0x4642_0002;
pub const FB_IOCTL_DRAW: u64 = 0x4642_0003;
pub const FB_IOCTL_SCREEN_INFO: u64 = 0x4642_0004;
pub const FB_IOCTL_FLIP: u64 = 0x4642_0005;
pub const FB_IOCTL_MMAP_INFO: u64 = 0x4642_0006;
pub const FB_IOCTL_SET_CURSOR: u64 = 0x4642_0007;
pub const FB_IOCTL_MOVE_CURSOR: u64 = 0x4642_0008;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferRect {
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
    pub color: u32,
    pub _padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferDraw {
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
    pub pixel_count: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
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
    pub _padding: [u8; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferSetCursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub pixel_count: u32,
    pub _padding: u32,
    // Followed by pixel_count u32 pixels (ARGB)
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferMoveCursor {
    pub x: u32,
    pub y: u32,
}

pub struct FramebufferDevice;

impl FramebufferDevice {
    pub fn register() {
        let device = Arc::new(Self);
        register_device_str("/fb", device).expect("failed to register framebuffer device");
    }
}

impl DevFsDevice for FramebufferDevice {
    fn ioctl(&self, request: u64, arg: u64) -> Result<u64, DevFsError> {
        match request {
            FB_IOCTL_RENDER => {
                // No-op: we write directly to the framebuffer in draw/draw_rect.
                Ok(0)
            }
            FB_IOCTL_DRAW_RECT => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                // arg points to kernel-copied ioctl buffer (safe to read)
                let rect = unsafe { &*(arg as *const FramebufferRect) };

                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                display
                    .lock()
                    .draw_rect(rect.x, rect.y, rect.width, rect.height, rect.color);
                Ok(0)
            }
            FB_IOCTL_DRAW => {
                // The generic ioctl layer has already copied the entire user buffer
                // (header + pixel data) into a kernel Vec<u8>. The `arg` pointer
                // points into that kernel buffer, so it is safe to read.
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let header = unsafe { *(arg as *const FramebufferDraw) };

                if header.width == 0 || header.height == 0 {
                    return Err(DevFsError::IoError);
                }

                let expected_pixels = header
                    .width
                    .checked_mul(header.height)
                    .ok_or(DevFsError::IoError)?;

                if header.pixel_count != expected_pixels {
                    return Err(DevFsError::IoError);
                }

                // Pixel data follows the header in the same kernel buffer.
                let data_ptr =
                    unsafe { (arg as *const u8).add(core::mem::size_of::<FramebufferDraw>()) };
                let pixels = unsafe {
                    core::slice::from_raw_parts(data_ptr as *const u32, header.pixel_count as usize)
                };

                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                display.lock().draw(
                    pixels,
                    header.x,
                    header.y,
                    header.width as usize,
                    header.height as usize,
                );
                Ok(0)
            }
            FB_IOCTL_SCREEN_INFO => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                let info = display.lock().screen_info();
                let info_ptr = arg as *mut FramebufferInfo;
                unsafe {
                    (*info_ptr).width = info.width as u32;
                    (*info_ptr).height = info.height as u32;
                }
                Ok(0)
            }
            FB_IOCTL_FLIP => {
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                Ok(display.lock().flip())
            }
            FB_IOCTL_MMAP_INFO => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                let info = display.lock().mmap_info();
                let info_ptr = arg as *mut FramebufferMmapInfo;
                unsafe {
                    *info_ptr = info;
                }
                Ok(0)
            }
            FB_IOCTL_SET_CURSOR => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let header = unsafe { *(arg as *const FramebufferSetCursor) };
                let expected = (header.width * header.height) as u64;
                if header.pixel_count as u64 != expected {
                    return Err(DevFsError::IoError);
                }
                let data_ptr =
                    unsafe { (arg as *const u8).add(core::mem::size_of::<FramebufferSetCursor>()) };
                let pixels = unsafe {
                    core::slice::from_raw_parts(data_ptr as *const u32, header.pixel_count as usize)
                };
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                display
                    .lock()
                    .set_cursor(pixels, header.hot_x, header.hot_y);
                Ok(0)
            }
            FB_IOCTL_MOVE_CURSOR => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let pos = unsafe { *(arg as *const FramebufferMoveCursor) };
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                display.lock().move_cursor(pos.x, pos.y);
                Ok(0)
            }
            _ => Err(DevFsError::Unsupported),
        }
    }
}
