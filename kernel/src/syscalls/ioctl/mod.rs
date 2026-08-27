use alloc::{vec, vec::Vec};

use crate::debug::lock_order::RANK_PTY;
use crate::ranked_lock;
use crate::thread::scheduler::current_thread_info;
use crate::{
    fs::{api as fs_api, devfs},
    syscalls::Errno,
    thread::pipe::FileDescriptor,
    util::uaccess::{try_copy_from_user, try_copy_to_user},
};
use x86_64::instructions::interrupts;

pub const IOCTL_FLAG_READ: u64 = 1;
pub const IOCTL_FLAG_WRITE: u64 = 1 << 1;

pub fn sys_ioctl(
    fd: u64,
    request: u64,
    arg: u64,
    arg_len: usize,
    flags: u64,
) -> Result<u64, Errno> {
    let info = current_thread_info();
    // Bind the lookup to a `let` before matching on it: temporaries in a match
    // scrutinee live until the end of the match, so matching on
    // `info.lock()...` directly would hold the thread-info IrqSpinlock while an
    // arm re-locks it, wedging the CPU with interrupts off.
    let fd_table = info.lock().fd_table.clone();
    let looked_up = fd_table.lock().get_fd(fd).cloned();

    let descriptor = match looked_up {
        Some(desc) => desc,
        None => {
            return Err(Errno::EBADF);
        }
    };

    match descriptor {
        FileDescriptor::PtyMaster(pty_arc) | FileDescriptor::PtySlave(pty_arc) => {
            ranked_lock!(RANK_PTY, "sys_ioctl::pty", pty_arc)
                .ioctl_with(request, arg)
                .map_err(|()| Errno::EINVAL)
        }
        FileDescriptor::FsFile(file) => {
            let copy_in = flags & IOCTL_FLAG_READ != 0;
            let copy_out = flags & IOCTL_FLAG_WRITE != 0;
            let need_buffer = arg_len > 0 && (copy_in || copy_out);

            // Fast path: bypass the FS Mailbox for devfs devices.
            // DevFsDevice::ioctl is thread-safe (behind Arc) and doesn't need the FS thread.
            let devfs_device = devfs::try_lookup_from_full_path(&file.path);

            if need_buffer {
                if arg == 0 {
                    return Err(Errno::EFAULT);
                }

                let user_ptr = arg as *mut u8;
                if user_ptr.is_null() {
                    return Err(Errno::EFAULT);
                }

                // A `u64` element type rather than `u8`, so the allocation is
                // 8-aligned: devices read structs and `u32` slices straight out
                // of it, and `Vec<u8>` promises alignment 1. The byte length
                // stays `arg_len`; the rounding only ever adds slack past the
                // end, which nothing is told about.
                let mut buffer: Vec<u64> = vec![0u64; arg_len.div_ceil(8)];
                let buf_ptr = buffer.as_mut_ptr() as *mut u8;

                if copy_in
                    // SAFETY: `buffer` holds `arg_len.div_ceil(8)` `u64`s,
                    // which is at least `arg_len` bytes, and every bit pattern
                    // of `u64` is valid so writing bytes into it is defined.
                    && !unsafe { try_copy_from_user(buf_ptr, user_ptr as *const u8, arg_len) }
                {
                    return Err(Errno::EFAULT);
                }

                interrupts::enable();

                let result = if let Some(ref device) = devfs_device {
                    device
                        .ioctl(request, buf_ptr as u64, arg_len)
                        .map(|v| v as i64)
                        .map_err(crate::fs::Error::from)
                } else {
                    fs_api::ioctl(&file.path, request, buf_ptr as u64, arg_len).map(|v| v as i64)
                };

                match result {
                    Ok(value) => {
                        if copy_out
                            // SAFETY: `buffer` is still the allocation made
                            // above and still at least `arg_len` bytes; the
                            // driver wrote into it in place and cannot have
                            // changed its length.
                            && !unsafe { try_copy_to_user(user_ptr, buf_ptr, arg_len) }
                        {
                            return Err(Errno::EFAULT);
                        }
                        Ok(value as u64)
                    }
                    Err(err) => Err(Errno::from(err)),
                }
            } else {
                interrupts::enable();

                // No buffer was copied, so `arg` is a scalar and there are no
                // bytes behind it: `arg_len` 0 tells the device exactly that,
                // whatever the caller passed.
                let result = if let Some(ref device) = devfs_device {
                    device
                        .ioctl(request, arg, 0)
                        .map(|v| v as i64)
                        .map_err(crate::fs::Error::from)
                } else {
                    fs_api::ioctl(&file.path, request, arg, 0).map(|v| v as i64)
                };

                match result {
                    Ok(value) => Ok(value as u64),
                    Err(err) => Err(Errno::from(err)),
                }
            }
        }
        _ => Err(Errno::EINVAL),
    }
}
