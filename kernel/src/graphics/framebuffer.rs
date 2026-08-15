use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use crate::{
    fs::{DevFsDevice, DevFsError, register_device_str},
    graphics::DISPLAY,
    interrupts::io::{VIRTIO_GPU_IRQS_FIRED, VIRTIO_GPU_WAITERS},
};

pub const FB_IOCTL_DRAW_RECT: u64 = 0x4642_0001;
pub const FB_IOCTL_RENDER: u64 = 0x4642_0002;
pub const FB_IOCTL_DRAW: u64 = 0x4642_0003;
pub const FB_IOCTL_SCREEN_INFO: u64 = 0x4642_0004;
pub const FB_IOCTL_FLIP: u64 = 0x4642_0005;
pub const FB_IOCTL_MMAP_INFO: u64 = 0x4642_0006;
pub const FB_IOCTL_SET_CURSOR: u64 = 0x4642_0007;
pub const FB_IOCTL_MOVE_CURSOR: u64 = 0x4642_0008;
pub const FB_IOCTL_FLIP_RECT: u64 = 0x4642_0009;
/// Wait until the previous flip's pixels have been read out of the framebuffer.
///
/// Its own call rather than part of the flip, because the two happen at
/// opposite ends of a frame: the flip submits, and the wait belongs immediately
/// before the compositor writes the buffer again. See [`Display::flip_wait`].
///
/// [`Display::flip_wait`]: crate::graphics::Display::flip_wait
pub const FB_IOCTL_FLIP_WAIT: u64 = 0x4642_000A;

/// How many times [`FB_IOCTL_FLIP_WAIT`] parks before giving up on the display,
/// and how long each park lasts. Sized so the total is far longer than any
/// frame and far shorter than a user waiting on a wedged desktop.
pub const FB_IOCTL_FLIP_RECTS: u64 = 0x4642_000B;

const FLIP_WAIT_ROUNDS: u32 = 8;
const FLIP_WAIT_SLICE: core::time::Duration = core::time::Duration::from_millis(4);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferFlipRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The regions of one frame, and the box that covers them.
///
/// Several transfers behind one flush: the host copies only what changed, and
/// still presents the frame in one piece. `bounds` is what gets flushed and so
/// must cover every rect in `rects`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferFlipRects {
    pub count: u32,
    pub bounds: FramebufferFlipRect,
    pub rects: [FramebufferFlipRect; crate::graphics::MAX_FLIP_RECTS],
}

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
    pub refresh_rate: u32,
    pub _padding: u32,
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
                    (*info_ptr).refresh_rate = info.refresh_rate;
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
            FB_IOCTL_FLIP_WAIT => {
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                // **The lock is held for the look, never for the wait.** The
                // queue lives behind `DISPLAY`, a preempt-disabling spinlock,
                // and parking under one is forbidden -- but only the poll needs
                // the lock. Dropping it before parking is what turns this from
                // a spin into a sleep.
                //
                // The interrupt count is the condition rather than the queue's
                // own state, because the queue cannot be read without the lock
                // and a parked thread holds none. A changed count means a
                // completion landed and the poll is worth repeating.
                for _ in 0..FLIP_WAIT_ROUNDS {
                    let seq = {
                        let mut d = display.lock();
                        if d.flip_poll() {
                            return Ok(0);
                        }
                        if !d.flip_has_irq() {
                            // Nothing will announce the completion, so there is
                            // nothing to park on. The driver's own bounded look
                            // is all that is left.
                            d.flip_wait();
                            return Ok(0);
                        }
                        VIRTIO_GPU_IRQS_FIRED.load(Ordering::Relaxed)
                    };
                    VIRTIO_GPU_WAITERS.wait_until_timeout(
                        || VIRTIO_GPU_IRQS_FIRED.load(Ordering::Relaxed) != seq,
                        Some(FLIP_WAIT_SLICE),
                    );
                }
                // A display that has not answered in this long is not going to
                // be waited into working. Let the frame through: the pixels may
                // tear, which is better than a compositor that never returns.
                Ok(0)
            }
            FB_IOCTL_FLIP_RECTS => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let list = unsafe { &*(arg as *const FramebufferFlipRects) };
                let n = (list.count as usize).min(crate::graphics::MAX_FLIP_RECTS);
                if n == 0 {
                    return Ok(0);
                }
                let mut rects = [(0u32, 0u32, 0u32, 0u32); crate::graphics::MAX_FLIP_RECTS];
                for (slot, r) in rects.iter_mut().zip(&list.rects[..n]) {
                    *slot = (r.x, r.y, r.width, r.height);
                }
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                Ok(display.lock().flip_rects(
                    &rects[..n],
                    (
                        list.bounds.x,
                        list.bounds.y,
                        list.bounds.width,
                        list.bounds.height,
                    ),
                ))
            }
            FB_IOCTL_FLIP_RECT => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let rect = unsafe { *(arg as *const FramebufferFlipRect) };
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                Ok(display
                    .lock()
                    .flip_rect(rect.x, rect.y, rect.width, rect.height))
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
                if !display
                    .lock()
                    .set_cursor(pixels, header.hot_x, header.hot_y)
                {
                    return Err(DevFsError::Unsupported);
                }
                Ok(0)
            }
            FB_IOCTL_MOVE_CURSOR => {
                if arg == 0 {
                    return Err(DevFsError::IoError);
                }
                let pos = unsafe { *(arg as *const FramebufferMoveCursor) };
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                if !display.lock().move_cursor(pos.x, pos.y) {
                    return Err(DevFsError::Unsupported);
                }
                Ok(0)
            }
            _ => Err(DevFsError::Unsupported),
        }
    }
}
