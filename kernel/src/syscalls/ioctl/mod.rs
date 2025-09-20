use crate::{
    fs::api as fs_api,
    syscalls::Errno,
    thread::{pipe::FileDescriptor, scheduler::sched},
};
use x86_64::instructions::interrupts;

pub mod framebuffer;

pub fn sys_ioctl(fd: u64, request: u64, arg: u64) -> i64 {
    let info = sched().current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    let descriptor = match thread.fd_table.get_fd(fd).cloned() {
        Some(desc) => desc,
        None => {
            thread.errno = Errno::EBADF;
            return -1;
        }
    };

    let FileDescriptor::FsFile(file) = descriptor else {
        thread.errno = Errno::EINVAL;
        return -1;
    };

    match framebuffer::try_handle(&file, request, arg) {
        Some(Ok(value)) => return value as i64,
        Some(Err(errno)) => {
            thread.errno = errno;
            return -1;
        }
        None => {}
    }

    interrupts::enable();
    match fs_api::ioctl(&file.path, request, arg) {
        Ok(value) => value as i64,
        Err(err) => {
            thread.errno = Errno::from(err);
            -1
        }
    }
}
