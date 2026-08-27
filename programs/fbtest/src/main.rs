//! The framebuffer ioctls, asked for more bytes than they were given.
//!
//! Every `/dev/fb` request reads a header, and three of them read a pixel
//! tail, out of a buffer whose size is a syscall argument. Nothing in the
//! syscall layer or in devfs can bound those reads, because only the device
//! knows the shape a request names, so the bound lives in the device and this
//! is what says it is still there. A case that passes an honest length first
//! proves the request itself works, so a refusal below means the length was
//! rejected and not that the device is missing.

use std::fs::File;
use std::os::fd::AsRawFd;

use edos_lib::sys::{SYS_IOCTL, syscall5};

const FB_IOCTL_DRAW_RECT: u64 = 0x4642_0001;
const FB_IOCTL_DRAW: u64 = 0x4642_0003;
const FB_IOCTL_SCREEN_INFO: u64 = 0x4642_0004;
const FB_IOCTL_MMAP_INFO: u64 = 0x4642_0006;
const FB_IOCTL_SET_CURSOR: u64 = 0x4642_0007;
const FB_IOCTL_MOVE_CURSOR: u64 = 0x4642_0008;
const FB_IOCTL_FLIP_RECT: u64 = 0x4642_0009;
const FB_IOCTL_TRACK_POINTER: u64 = 0x4642_000C;

const ARG_IN: u64 = 1;
const ARG_OUT: u64 = 1 << 1;

/// `FramebufferDraw`: x, y, width, height, pixel_count, all `u64`.
const DRAW_HEADER: usize = 40;
/// `FramebufferSetCursor`: five `u32` and a pad word.
const CURSOR_HEADER: usize = 24;
/// `FramebufferInfo`: width, height, refresh_rate and a pad word.
const INFO: usize = 16;

fn ioctl(fd: u64, request: u64, buf: &mut [u8], len: usize, flags: u64) -> Result<u64, i64> {
    // SAFETY: `buf` is a live allocation of at least `len` bytes in every call
    // below, which is what the kernel copies in and out.
    let ret = unsafe {
        syscall5(
            SYS_IOCTL,
            fd,
            request,
            buf.as_mut_ptr() as u64,
            len as u64,
            flags,
        )
    } as i64;
    if (-4095..0).contains(&ret) {
        Err(ret)
    } else {
        Ok(ret as u64)
    }
}

struct Runner {
    fd: u64,
    failures: u32,
    passes: u32,
}

impl Runner {
    /// A request the kernel must refuse: the buffer is shorter than the shape
    /// the request names.
    fn refuses(&mut self, name: &str, request: u64, buf: &mut [u8], len: usize, flags: u64) {
        match ioctl(self.fd, request, buf, len, flags) {
            Err(_) => {
                self.passes += 1;
                println!("PASS {name}");
            }
            Ok(v) => {
                self.failures += 1;
                println!("FAIL {name}: accepted a {len}-byte buffer and answered {v}");
            }
        }
    }

    fn accepts(&mut self, name: &str, request: u64, buf: &mut [u8], len: usize, flags: u64) {
        match ioctl(self.fd, request, buf, len, flags) {
            Ok(_) => {
                self.passes += 1;
                println!("PASS {name}");
            }
            Err(e) => {
                self.failures += 1;
                println!("FAIL {name}: an honest {len}-byte buffer was refused with {e}");
            }
        }
    }
}

fn main() {
    let file = match File::open("/dev/fb") {
        Ok(f) => f,
        Err(e) => {
            eprintln!("fbtest: cannot open /dev/fb: {e}");
            std::process::exit(1);
        }
    };
    let mut r = Runner {
        fd: file.as_raw_fd() as u64,
        failures: 0,
        passes: 0,
    };

    // Larger than the longest length any case passes: the kernel copies
    // `arg_len` bytes in from this pointer, so a case that names more than the
    // buffer holds would have the kernel reading off the end of it, and a
    // buffer that happened to sit at the end of a page would make the case
    // fail with `EFAULT` for a reason it is not about.
    let mut buf = [0u8; 512];

    // The control. Without it every refusal below could just as well mean the
    // device is not there.
    r.accepts(
        "screen-info sized",
        FB_IOCTL_SCREEN_INFO,
        &mut buf,
        INFO,
        ARG_OUT,
    );
    let width = u32::from_ne_bytes(buf[0..4].try_into().unwrap());
    let height = u32::from_ne_bytes(buf[4..8].try_into().unwrap());
    if width == 0 || height == 0 {
        println!("FAIL screen-info sized: answered a {width}x{height} screen");
        r.failures += 1;
    }

    // A request that writes its answer through the pointer, given less room
    // than it writes.
    r.refuses(
        "screen-info short",
        FB_IOCTL_SCREEN_INFO,
        &mut buf,
        4,
        ARG_OUT,
    );
    r.refuses("mmap-info short", FB_IOCTL_MMAP_INFO, &mut buf, 8, ARG_OUT);

    // Fixed-size headers, read out of a buffer too small to hold one.
    r.refuses("draw-rect short", FB_IOCTL_DRAW_RECT, &mut buf, 4, ARG_IN);
    r.refuses("flip-rect short", FB_IOCTL_FLIP_RECT, &mut buf, 8, ARG_IN);
    r.refuses(
        "move-cursor short",
        FB_IOCTL_MOVE_CURSOR,
        &mut buf,
        4,
        ARG_IN,
    );
    r.refuses(
        "track-pointer short",
        FB_IOCTL_TRACK_POINTER,
        &mut buf,
        2,
        ARG_IN,
    );

    // The variable-length pair: a header that is internally consistent about
    // an 8x8 rectangle, in a buffer holding the header and not one pixel.
    // Before the length reached the device this built a 64-`u32` slice over
    // the end of the allocation; a big enough rectangle put the kernel heap on
    // screen and then faulted.
    buf.fill(0);
    buf[16..24].copy_from_slice(&8u64.to_ne_bytes()); // width
    buf[24..32].copy_from_slice(&8u64.to_ne_bytes()); // height
    buf[32..40].copy_from_slice(&64u64.to_ne_bytes()); // pixel_count
    r.refuses(
        "draw header-only",
        FB_IOCTL_DRAW,
        &mut buf,
        DRAW_HEADER,
        ARG_IN,
    );
    r.refuses(
        "draw one pixel short",
        FB_IOCTL_DRAW,
        &mut buf,
        DRAW_HEADER + 63 * 4,
        ARG_IN,
    );

    buf.fill(0);
    buf[0..4].copy_from_slice(&8u32.to_ne_bytes()); // width
    buf[4..8].copy_from_slice(&8u32.to_ne_bytes()); // height
    buf[16..20].copy_from_slice(&64u32.to_ne_bytes()); // pixel_count
    r.refuses(
        "set-cursor header-only",
        FB_IOCTL_SET_CURSOR,
        &mut buf,
        CURSOR_HEADER,
        ARG_IN,
    );
    r.refuses(
        "set-cursor one pixel short",
        FB_IOCTL_SET_CURSOR,
        &mut buf,
        CURSOR_HEADER + 63 * 4,
        ARG_IN,
    );

    if r.failures == 0 {
        println!("fbtest: all {} cases passed", r.passes);
    } else {
        println!(
            "fbtest: {} of {} cases failed",
            r.failures,
            r.passes + r.failures
        );
        std::process::exit(1);
    }
}
