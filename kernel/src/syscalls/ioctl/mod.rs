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

pub fn sys_ioctl(fd: u64, request: u64, arg: u64, arg_len: usize, flags: u64) -> i64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    // Bind the lookup to a `let` before matching on it: temporaries in a match
    // scrutinee live until the end of the match, so matching on
    // `info.lock()...` directly would hold the thread-info IrqSpinlock while an
    // arm re-locks it, wedging the CPU with interrupts off.
    let fd_table = info.lock().fd_table.clone();
    let looked_up = fd_table.lock().get_fd(fd).cloned();

    let descriptor = match looked_up {
        Some(desc) => desc,
        None => {
            info.lock().errno = Errno::EBADF;
            return -1;
        }
    };

    match descriptor {
        FileDescriptor::PtyMaster(pty_arc) | FileDescriptor::PtySlave(pty_arc) => {
            match ranked_lock!(RANK_PTY, "sys_ioctl::pty", pty_arc).ioctl_with(request, arg) {
                Ok(val) => val as i64,
                Err(()) => {
                    info.lock().errno = Errno::EINVAL;
                    -1
                }
            }
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
                    info.lock().errno = Errno::EFAULT;
                    return -1;
                }

                let user_ptr = arg as *mut u8;
                if user_ptr.is_null() {
                    info.lock().errno = Errno::EFAULT;
                    return -1;
                }

                let mut buffer: Vec<u8> = vec![0u8; arg_len];

                if copy_in
                    && !unsafe {
                        try_copy_from_user(buffer.as_mut_ptr(), user_ptr as *const u8, arg_len)
                    }
                {
                    info.lock().errno = Errno::EFAULT;
                    return -1;
                }

                interrupts::enable();

                let result = if let Some(ref device) = devfs_device {
                    device
                        .ioctl(request, buffer.as_mut_ptr() as u64)
                        .map(|v| v as i64)
                        .map_err(crate::fs::Error::from)
                } else {
                    fs_api::ioctl(&file.path, request, buffer.as_mut_ptr() as u64).map(|v| v as i64)
                };

                match result {
                    Ok(value) => {
                        if copy_out
                            && !unsafe { try_copy_to_user(user_ptr, buffer.as_ptr(), arg_len) }
                        {
                            info.lock().errno = Errno::EFAULT;
                            return -1;
                        }
                        value
                    }
                    Err(err) => {
                        info.lock().errno = Errno::from(err);
                        -1
                    }
                }
            } else {
                interrupts::enable();

                let result = if let Some(ref device) = devfs_device {
                    device
                        .ioctl(request, arg)
                        .map(|v| v as i64)
                        .map_err(crate::fs::Error::from)
                } else {
                    fs_api::ioctl(&file.path, request, arg).map(|v| v as i64)
                };

                match result {
                    Ok(value) => value,
                    Err(err) => {
                        info.lock().errno = Errno::from(err);
                        -1
                    }
                }
            }
        }
        _ => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}
