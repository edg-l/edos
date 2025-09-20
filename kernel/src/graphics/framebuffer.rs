use alloc::{boxed::Box, sync::Arc};

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
    pub pixels: *const u32,
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
}

pub struct FramebufferDrawCommand {
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
    pub pixels: Box<[u32]>,
}

impl FramebufferDrawCommand {
    pub fn into_draw_request(self) -> DrawRequest {
        DrawRequest {
            pixels: self.pixels,
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
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
                let rect = unsafe { &*(arg as *const FramebufferRect) };
                api::draw_rect(rect.x, rect.y, rect.width, rect.height, rect.color);
                Ok(0)
            }
            FB_IOCTL_DRAW => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let command_ptr = arg as *mut FramebufferDrawCommand;
                let command = unsafe { Box::from_raw(command_ptr) };
                let request = command.into_draw_request();
                api::draw(request);
                Ok(0)
            }
            FB_IOCTL_SCREEN_INFO => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let info_ptr = arg as *mut FramebufferInfo;
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
