use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use crate::{
    fs::{DevFsDevice, DevFsError, register_device_str},
    graphics::{CURSOR_STALE_MOVES, CURSOR_TRACKS_POINTER, DISPLAY},
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

/// Ask the display to keep its cursor plane on the pointer by itself.
///
/// `arg` points at a `u32`: non-zero enables, zero disables. A caller that
/// enables this stops needing a move per frame, and the plane is placed from
/// the input path as each report lands instead — which is the whole latency
/// difference between a pointer that lags the compositor and one that does
/// not. Uploading a new image with [`FB_IOCTL_SET_CURSOR`] leaves the plane
/// where it is, so a shape change owes no move.
///
/// A caller that keeps placing the plane while this is on is competing with the
/// input path rather than helping it: its position is a frame old by
/// construction, and placing it walks the plane back off the pointer.
/// `cursor_stale_moves` in `/proc/gpu_stats` counts that happening.
pub const FB_IOCTL_TRACK_POINTER: u64 = 0x4642_000C;

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

/// The ioctl buffer, as a device is allowed to see it.
///
/// `sys_ioctl` hands a device a pointer and a byte count, and userspace chose
/// the count: every read out of the buffer has to be bounded by it, and the
/// tail of a variable-length request has to be bounded by what the header
/// leaves. Reading a header without checking `len` first is how a caller with
/// `arg_len = 0` gets the kernel heap blitted to the screen.
struct IoctlBuf {
    ptr: *const u8,
    len: usize,
}

impl IoctlBuf {
    /// # Safety
    ///
    /// `arg` must be the buffer `sys_ioctl` copied the caller's bytes into and
    /// `arg_len` its length, per [`DevFsDevice::ioctl`]: `arg_len` bytes
    /// readable and writable, alive for the call, aligned to 8.
    ///
    /// A null `arg` is the scalar case. It is recorded as an empty buffer
    /// rather than refused, so the requests that take no buffer still work and
    /// every request that does read one fails its own length check.
    unsafe fn new(arg: u64, arg_len: usize) -> Self {
        Self {
            ptr: arg as *const u8,
            len: if arg == 0 { 0 } else { arg_len },
        }
    }

    /// The request's fixed-size header, by value.
    fn header<T: Copy>(&self) -> Result<T, DevFsError> {
        if self.len < size_of::<T>() {
            return Err(DevFsError::IoError);
        }
        // SAFETY: the buffer holds at least `size_of::<T>()` bytes, checked
        // immediately above, and `sys_ioctl` aligned it to 8, which every
        // header here needs at most. `T` is `Copy` and every one used with this
        // is a `#[repr(C)]` struct of integers, so any bit pattern the caller
        // wrote is a valid value.
        Ok(unsafe { (self.ptr as *const T).read() })
    }

    /// The `count` `u32`s following a `T` header, checked against the bytes the
    /// caller actually passed.
    fn tail_u32<T>(&self, count: usize) -> Result<&[u32], DevFsError> {
        let head = size_of::<T>();
        let bytes = count
            .checked_mul(size_of::<u32>())
            .ok_or(DevFsError::IoError)?;
        if head.checked_add(bytes).ok_or(DevFsError::IoError)? > self.len {
            return Err(DevFsError::IoError);
        }
        // SAFETY: `head + count * 4` bytes are inside the buffer, checked
        // immediately above, so the slice lies entirely within one allocation.
        // `sys_ioctl` aligned that allocation to 8 and `head` is a multiple of
        // 4 in every header this is used with, so the tail is 4-aligned; every
        // bit pattern of `u32` is valid. The borrow ends before `ioctl`
        // returns, and the buffer outlives the call.
        Ok(unsafe { core::slice::from_raw_parts(self.ptr.add(head) as *const u32, count) })
    }

    /// The buffer as a `T` to write the answer into.
    fn out<T>(&self) -> Result<*mut T, DevFsError> {
        if self.len < size_of::<T>() {
            return Err(DevFsError::IoError);
        }
        Ok(self.ptr as *mut T)
    }
}

impl DevFsDevice for FramebufferDevice {
    fn ioctl(&self, request: u64, arg: u64, arg_len: usize) -> Result<u64, DevFsError> {
        // SAFETY: `arg` and `arg_len` are this call's own parameters, and
        // `DevFsDevice::ioctl` documents them as exactly the buffer and length
        // `IoctlBuf` asks for.
        let buf = unsafe { IoctlBuf::new(arg, arg_len) };

        match request {
            FB_IOCTL_RENDER => {
                // No-op: we write directly to the framebuffer in draw/draw_rect.
                Ok(0)
            }
            FB_IOCTL_DRAW_RECT => {
                let rect: FramebufferRect = buf.header()?;
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                display
                    .lock()
                    .draw_rect(rect.x, rect.y, rect.width, rect.height, rect.color);
                Ok(0)
            }
            FB_IOCTL_DRAW => {
                let header: FramebufferDraw = buf.header()?;

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

                // The pixels follow the header in the same buffer. `tail_u32`
                // is what says they are really there: the count agreeing with
                // `width * height` says only that the caller is consistent
                // about a rectangle it may never have sent.
                let count = usize::try_from(header.pixel_count).map_err(|_| DevFsError::IoError)?;
                let pixels = buf.tail_u32::<FramebufferDraw>(count)?;

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
                let out = buf.out::<FramebufferInfo>()?;
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                let info = display.lock().screen_info();
                // SAFETY: `out` checked the buffer holds a whole
                // `FramebufferInfo` and `sys_ioctl` aligned it; the write is
                // the only reference to it in this call.
                unsafe {
                    (*out).width = info.width as u32;
                    (*out).height = info.height as u32;
                    (*out).refresh_rate = info.refresh_rate;
                }
                Ok(0)
            }
            FB_IOCTL_FLIP => {
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                Ok(display.lock().flip())
            }
            FB_IOCTL_MMAP_INFO => {
                let out = buf.out::<FramebufferMmapInfo>()?;
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                let info = display.lock().mmap_info();
                // SAFETY: `out` checked the buffer holds a whole
                // `FramebufferMmapInfo` and `sys_ioctl` aligned it; the write
                // is the only reference to it in this call.
                unsafe {
                    *out = info;
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
                let list: FramebufferFlipRects = buf.header()?;
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
                let rect: FramebufferFlipRect = buf.header()?;
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                Ok(display
                    .lock()
                    .flip_rect(rect.x, rect.y, rect.width, rect.height))
            }
            FB_IOCTL_SET_CURSOR => {
                let header: FramebufferSetCursor = buf.header()?;
                let expected = header
                    .width
                    .checked_mul(header.height)
                    .ok_or(DevFsError::IoError)?;
                if header.pixel_count != expected {
                    return Err(DevFsError::IoError);
                }
                let pixels = buf.tail_u32::<FramebufferSetCursor>(header.pixel_count as usize)?;
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
                let pos: FramebufferMoveCursor = buf.header()?;
                // A placement that disagrees with where the pointer is, while
                // the display is following the pointer itself, moves the plane
                // off the pointer until the next report arrives to correct it.
                // The caller is a frame behind the input path by construction,
                // so this counts how often it is competing rather than
                // repairing.
                if CURSOR_TRACKS_POINTER.load(Ordering::Relaxed) {
                    let (px, py) = crate::drivers::mouse::get_position();
                    if pos.x as i32 != px || pos.y as i32 != py {
                        CURSOR_STALE_MOVES.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if !crate::graphics::cursor_plane_elsewhere(pos.x, pos.y) {
                    return Ok(0);
                }
                let display = DISPLAY.get().ok_or(DevFsError::IoError)?;
                if !display.lock().move_cursor(pos.x, pos.y) {
                    return Err(DevFsError::Unsupported);
                }
                crate::graphics::cursor_plane_placed(pos.x, pos.y);
                Ok(0)
            }
            FB_IOCTL_TRACK_POINTER => {
                let enable: u32 = buf.header()?;
                // Not validated against the display having a plane: a display
                // without one answers `false` from `move_cursor` and the
                // tracking store is inert, and the caller learned which it has
                // from `FB_IOCTL_SET_CURSOR` before asking.
                CURSOR_TRACKS_POINTER.store(enable != 0, Ordering::Relaxed);
                Ok(0)
            }
            _ => Err(DevFsError::Unsupported),
        }
    }
}
