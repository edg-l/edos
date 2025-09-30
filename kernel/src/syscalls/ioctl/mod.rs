use alloc::{vec, vec::Vec};

use crate::{
    fs::api as fs_api,
    syscalls::Errno,
    thread::{pipe::FileDescriptor, scheduler::sched},
};
use x86_64::instructions::interrupts;

pub const IOCTL_FLAG_READ: u64 = 1;
pub const IOCTL_FLAG_WRITE: u64 = 1 << 1;

pub fn sys_ioctl(fd: u64, request: u64, arg: u64, arg_len: usize, flags: u64) -> i64 {
    let info = sched().current_thread_info();
    info.lock().errno = Errno::Clear;

    let descriptor = match info.lock().fd_table.lock().get_fd(fd).cloned() {
        Some(desc) => desc,
        None => {
            info.lock().errno = Errno::EBADF;
            return -1;
        }
    };

    let FileDescriptor::FsFile(file) = descriptor else {
        info.lock().errno = Errno::EINVAL;
        return -1;
    };

    let copy_in = flags & IOCTL_FLAG_READ != 0;
    let copy_out = flags & IOCTL_FLAG_WRITE != 0;
    let need_buffer = arg_len > 0 && (copy_in || copy_out);

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

        if copy_in {
            unsafe {
                core::ptr::copy_nonoverlapping(user_ptr as *const u8, buffer.as_mut_ptr(), arg_len);
            }
        }

        interrupts::enable();

        match fs_api::ioctl(&file.path, request, buffer.as_mut_ptr() as u64) {
            Ok(value) => {
                if copy_out {
                    unsafe {
                        core::ptr::copy_nonoverlapping(buffer.as_ptr(), user_ptr, arg_len);
                    }
                }
                value as i64
            }
            Err(err) => {
                info.lock().errno = Errno::from(err);
                -1
            }
        }
    } else {
        interrupts::enable();
        match fs_api::ioctl(&file.path, request, arg) {
            Ok(value) => value as i64,
            Err(err) => {
                info.lock().errno = Errno::from(err);
                -1
            }
        }
    }
}
