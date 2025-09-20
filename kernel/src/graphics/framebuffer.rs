use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{DevFsDevice, DevFsError, register_device_str},
    graphics::api::{self, DrawRequest},
};

pub const FB_IOCTL_DRAW_RECT: u64 = 0x4642_0001;
pub const FB_IOCTL_RENDER: u64 = 0x4642_0002;
pub const FB_IOCTL_DRAW: u64 = 0x4642_0003;
pub const FB_IOCTL_SCREEN_INFO: u64 = 0x4642_0004;

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
                api::render();
                Ok(0)
            }
            FB_IOCTL_DRAW_RECT => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let rect_ptr = arg as *const FramebufferRect;
                if rect_ptr.is_null() {
                    return Err(DevFsError::IoError);
                }
                let rect = unsafe { &*rect_ptr };
                api::draw_rect(rect.x, rect.y, rect.width, rect.height, rect.color);
                Ok(0)
            }
            FB_IOCTL_DRAW => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let draw_ptr = arg as *const FramebufferDraw;
                if draw_ptr.is_null() {
                    return Err(DevFsError::IoError);
                }
                let header = unsafe { *draw_ptr };

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

                let expected_len = header
                    .pixel_count
                    .checked_mul(4)
                    .ok_or(DevFsError::IoError)? as usize;

                let data_ptr =
                    unsafe { (arg as *const u8).add(core::mem::size_of::<FramebufferDraw>()) };
                let pixels_slice = unsafe {
                    core::slice::from_raw_parts(data_ptr as *const u32, header.pixel_count as usize)
                };

                if core::mem::size_of_val(pixels_slice) != expected_len {
                    return Err(DevFsError::IoError);
                }

                let mut pixels = Vec::with_capacity(pixels_slice.len());
                pixels.extend_from_slice(pixels_slice);

                let request = DrawRequest {
                    pixels: pixels.into_boxed_slice(),
                    x: header.x,
                    y: header.y,
                    width: header.width,
                    height: header.height,
                };

                api::draw(request);
                Ok(0)
            }
            FB_IOCTL_SCREEN_INFO => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let info_ptr = arg as *mut FramebufferInfo;
                if info_ptr.is_null() {
                    return Err(DevFsError::IoError);
                }
                let info = api::screen_info();
                unsafe {
                    (*info_ptr).width = info.width as u32;
                    (*info_ptr).height = info.height as u32;
                }
                Ok(0)
            }
            _ => Err(DevFsError::Unsupported),
        }
    }
}
