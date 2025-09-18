use alloc::vec::Vec;
use bytemuck::{NoUninit, Pod, Zeroable};
use x86_64::instructions::interrupts;

use crate::{
    fs::{
        Error,
        api::{list_partitions, mount_partition},
        gpt::Partition,
    },
    syscalls::io::resolve_path,
    thread::scheduler::sched,
};

use super::Errno;

pub fn sys_mount(device_id: u64, partition_idx: u64, path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    if path_ptr.is_null() {
        thread.errno = Errno::EFAULT;
        return -1;
    }

    // Copy C string from user memory (simple, bounded)
    let mut buf = alloc::vec::Vec::new();
    for i in 0..1024usize {
        let c = unsafe { core::ptr::read_volatile(path_ptr.add(i)) };
        if c == 0 {
            break;
        }
        buf.push(c);
    }
    // If no null terminator within bound, treat as invalid
    if buf.is_empty() || buf.len() == 1024 {
        thread.errno = Errno::EINVAL;
        return -1;
    }

    let path_str = match core::str::from_utf8(&buf) {
        Ok(s) => s,
        Err(_) => {
            thread.errno = Errno::EINVAL;
            return -1;
        }
    };

    let mount_point = match resolve_path(path_str, &thread.cwd) {
        Ok(path) => path,
        Err(_) => {
            thread.errno = Errno::EINVAL;
            return -1;
        }
    };

    interrupts::enable();

    // TODO: check mount point is a real folder and empty.

    match mount_partition(device_id as usize, partition_idx as usize, mount_point) {
        Ok(_) => 0,
        Err(err) => {
            thread.errno = Errno::from(err);
            -1
        }
    }
}

#[repr(C)]
#[derive(Debug, Zeroable, Pod, Clone, Copy)]
struct SysPartition {
    pub index: usize,
    pub starting_lba: u64,
    pub ending_lba: u64,
    pub size_sectors: u64,
    pub device_id: u64,
    pub unique_partition_guid: [u8; 16],
}

pub fn sys_list_partitions(buffer: *mut u8, size: u64) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    if buffer.is_null() {
        thread.errno = Errno::EFAULT;
        return -1;
    }

    interrupts::enable();

    let partitions = list_partitions();

    let mut current_ptr = buffer;
    let mut written = 0;

    for p in partitions {
        let x = SysPartition {
            index: p.index,
            device_id: p.device_id,
            ending_lba: p.ending_lba,
            size_sectors: p.size_sectors,
            starting_lba: p.starting_lba,
            unique_partition_guid: p.unique_partition_guid,
        };

        let bytes = bytemuck::bytes_of(&x);

        if written + bytes.len() > size as usize {
            break;
        }

        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), current_ptr, bytes.len());
        }
        written += bytes.len();
        current_ptr = unsafe { current_ptr.add(bytes.len()) };
    }

    written as i64
}
