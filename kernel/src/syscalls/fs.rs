use alloc::vec::Vec;
use bytemuck::{NoUninit, Pod, Zeroable};
use x86_64::instructions::interrupts;

use crate::{
    fs::{
        Error, FileKind,
        api::{
            create_dir, file_info, list_files, list_partitions, mount_partition, remove_dir,
            remove_file,
        },
        gpt::Partition,
        path::Path,
    },
    syscalls::io::resolve_path,
    thread::scheduler::sched,
};

use super::Errno;

const MAX_PATH_LEN: usize = 1024;

fn read_user_path(path_ptr: *const u8, cwd: &Path) -> Result<Path, Errno> {
    if path_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let mut buf = Vec::new();
    for i in 0..MAX_PATH_LEN {
        let c = unsafe { core::ptr::read_volatile(path_ptr.add(i)) };
        if c == 0 {
            break;
        }
        buf.push(c);
    }

    if buf.is_empty() || buf.len() == MAX_PATH_LEN {
        return Err(Errno::EINVAL);
    }

    let path_str = core::str::from_utf8(&buf).map_err(|_| Errno::EINVAL)?;
    resolve_path(path_str, cwd).map_err(|_| Errno::EINVAL)
}

fn remove_dir_recursive(path: &Path) -> Result<(), Error> {
    let entries = list_files(path)?;

    for entry in entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }

        let child_path = path.join(entry.name.as_str()).normalize();

        match entry.kind {
            FileKind::Directory => remove_dir_recursive(&child_path)?,
            _ => remove_file(&child_path)?,
        }
    }

    remove_dir(path)
}

pub fn sys_mount(device_id: u64, partition_idx: u64, path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    let mount_point = match read_user_path(path_ptr, &thread.cwd) {
        Ok(path) => path,
        Err(errno) => {
            thread.errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    let info = match file_info(&mount_point) {
        Ok(info) => info,
        Err(Error::FileNotFound) => {
            thread.errno = Errno::ENOENT;
            return -1;
        }
        Err(err) => {
            thread.errno = Errno::from(err);
            return -1;
        }
    };

    if info.kind != FileKind::Directory {
        thread.errno = Errno::ENOTDIR;
        return -1;
    }

    match list_files(&mount_point) {
        Ok(entries) => {
            let has_real_entries = entries
                .iter()
                .any(|entry| entry.name != "." && entry.name != "..");

            if has_real_entries {
                thread.errno = Errno::EEXIST;
                return -1;
            }
        }
        Err(err) => {
            thread.errno = Errno::from(err);
            return -1;
        }
    }

    // TODO: possible TOCTOU here.

    match mount_partition(device_id as usize, partition_idx as usize, mount_point) {
        Ok(_) => 0,
        Err(err) => {
            thread.errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_mkdir(path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    let path = match read_user_path(path_ptr, &thread.cwd) {
        Ok(path) => path,
        Err(errno) => {
            thread.errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    match create_dir(&path) {
        Ok(_) => 0,
        Err(err) => {
            thread.errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_rmdir(path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    let path = match read_user_path(path_ptr, &thread.cwd) {
        Ok(path) => path,
        Err(errno) => {
            thread.errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    match remove_dir(&path) {
        Ok(_) => 0,
        Err(err) => {
            thread.errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_rmdir_all(path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    let path = match read_user_path(path_ptr, &thread.cwd) {
        Ok(path) => path,
        Err(errno) => {
            thread.errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    match remove_dir_recursive(&path) {
        Ok(_) => 0,
        Err(err) => {
            thread.errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_unlink(path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    let path = match read_user_path(path_ptr, &thread.cwd) {
        Ok(path) => path,
        Err(errno) => {
            thread.errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    match remove_file(&path) {
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
