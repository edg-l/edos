use alloc::vec::Vec;

use crate::{Errno, SYS_DRAW, SYS_DRAW_RECT, SYS_RENDER, SYS_SCREEN_INFO, sys_errno, syscall};

pub fn sys_draw_rect(x: u64, y: u64, width: u64, height: u64, color: u32) -> i32 {
    let result = unsafe { syscall::syscall5(SYS_DRAW_RECT, x, y, width, height, color as u64) };
    if result == !0u64 {
        -1 // Check sys_errno() for details
    } else {
        0
    }
}

pub fn sys_render() -> i32 {
    let result = unsafe { syscall::syscall0(SYS_RENDER) };
    if result == !0u64 { -1 } else { 0 }
}

#[repr(C)]
pub struct ScreenInfo {
    pub width: usize,
    pub height: usize,
}

pub fn sys_screen_info() -> Result<ScreenInfo, Errno> {
    let mut info = ScreenInfo {
        width: 0,
        height: 0,
    };
    let result = unsafe { syscall::syscall1(SYS_SCREEN_INFO, &mut info as *mut _ as u64) };

    if result == !0u64 {
        Err(sys_errno())
    } else {
        Ok(info)
    }
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct DrawRequest {
    pub pixels: Vec<u32>,
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
}

pub fn sys_draw(request: &DrawRequest) -> i32 {
    let result = unsafe { syscall::syscall1(SYS_DRAW, request as *const _ as u64) };
    if result == !0u64 { -1 } else { 0 }
}
