use alloc::vec;
use alloc::{ffi::CString, string::ToString};
use bytemuck::{Pod, Zeroable};
use x86_64::instructions::interrupts;

use crate::{
    fs::{
        Error, FileKind,
        api::{
            create_dir, file_info, list_files, list_mounts, list_partitions, mount_partition,
            remove_dir, remove_file,
        },
        gpt::FilesystemType,
        path::Path,
    },
    syscalls::io::resolve_path,
    thread::{pipe::FileDescriptor, scheduler::sched},
    util::uaccess::{
        UAccessError, try_copy_from_user, try_copy_string_from_user, try_copy_to_user,
        try_write_user,
    },
};

use super::Errno;

const MAX_PATH_LEN: usize = 1024;

fn read_user_path(path_ptr: *const u8, cwd: &Path) -> Result<Path, Errno> {
    if path_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let mut buf = vec![0u8; MAX_PATH_LEN];
    let len = match unsafe { try_copy_string_from_user(buf.as_mut_ptr(), path_ptr, MAX_PATH_LEN) } {
        Ok(len) => len,
        Err(UAccessError::TooLong) => return Err(Errno::EINVAL),
        Err(UAccessError::Fault) => return Err(Errno::EFAULT),
    };

    if len == 0 {
        return Err(Errno::EINVAL);
    }

    buf.truncate(len);

    let path_str = core::str::from_utf8(&buf).map_err(|_| Errno::EINVAL)?;
    resolve_path(path_str, cwd).map_err(|_| Errno::EINVAL)
}

fn read_user_path_with_len(
    path_ptr: *const u8,
    path_len: usize,
    cwd: &Path,
) -> Result<Path, Errno> {
    if path_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    if path_len == 0 || path_len > MAX_PATH_LEN {
        return Err(Errno::EINVAL);
    }

    let mut buf = vec![0u8; path_len];
    if !unsafe { try_copy_from_user(buf.as_mut_ptr(), path_ptr, path_len) } {
        return Err(Errno::EFAULT);
    }

    let path_str = core::str::from_utf8(&buf).map_err(|_| Errno::EINVAL)?;
    resolve_path(path_str, cwd).map_err(|_| Errno::EINVAL)
}

fn read_user_str(value_ptr: *const u8) -> Result<CString, Errno> {
    if value_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let mut buf = vec![0u8; MAX_PATH_LEN];
    let len = match unsafe { try_copy_string_from_user(buf.as_mut_ptr(), value_ptr, MAX_PATH_LEN) }
    {
        Ok(len) => len,
        Err(UAccessError::TooLong) => return Err(Errno::EINVAL),
        Err(UAccessError::Fault) => return Err(Errno::EFAULT),
    };

    buf.truncate(len);
    CString::new(buf).map_err(|_| Errno::EINVAL)
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

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RawMountEntry {
    path_len: u32,
    filesystem: u32,
    device_id: u64,
    partition_index: u64,
}

fn filesystem_type_to_u32(fs: FilesystemType) -> u32 {
    match fs {
        FilesystemType::Unknown => 0,
        FilesystemType::Fat12 => 1,
        FilesystemType::Fat16 => 2,
        FilesystemType::Fat32 => 3,
        FilesystemType::Ntfs => 4,
        FilesystemType::Iso9660 => 5,
        FilesystemType::Memfs => 6,
        FilesystemType::Devfs => 7,
        FilesystemType::Procfs => 8,
    }
}

pub fn sys_mount(
    device_id: u64,
    partition_idx: u64,
    path_ptr: *const u8,
    fs_type: *const u8,
) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let cwd = info.lock().cwd.lock().clone();
    let mount_point = match read_user_path(path_ptr, &cwd) {
        Ok(path) => path,
        Err(errno) => {
            info.lock().errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    let finfo = match file_info(&mount_point) {
        Ok(info) => info,
        Err(Error::FileNotFound) => {
            info.lock().errno = Errno::ENOENT;
            return -1;
        }
        Err(err) => {
            info.lock().errno = Errno::from(err);
            return -1;
        }
    };

    if finfo.kind != FileKind::Directory {
        info.lock().errno = Errno::ENOTDIR;
        return -1;
    }

    let fs_type = match read_user_str(fs_type) {
        Ok(x) => x,
        Err(err) => {
            info.lock().errno = err;
            return -1;
        }
    }
    .to_string_lossy()
    .to_string();

    let fs_type = match fs_type.as_str() {
        "fat32" => FilesystemType::Fat32,
        "memfs" => FilesystemType::Memfs,
        "devfs" => FilesystemType::Devfs,
        "procfs" => FilesystemType::Procfs,
        _ => FilesystemType::Unknown,
    };

    match mount_partition(
        device_id as usize,
        partition_idx as usize,
        mount_point,
        fs_type,
    ) {
        Ok(_) => 0,
        Err(err) => {
            info.lock().errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_mkdir(path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let cwd = info.lock().cwd.lock().clone();
    let path = match read_user_path(path_ptr, &cwd) {
        Ok(path) => path,
        Err(errno) => {
            info.lock().errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    match create_dir(&path) {
        Ok(_) => 0,
        Err(err) => {
            info.lock().errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_rmdir(path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let cwd = info.lock().cwd.lock().clone();
    let path = match read_user_path(path_ptr, &cwd) {
        Ok(path) => path,
        Err(errno) => {
            info.lock().errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    match remove_dir(&path) {
        Ok(_) => 0,
        Err(err) => {
            info.lock().errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_rmdir_all(path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    let cwd = info.lock().cwd.lock().clone();
    let path = match read_user_path(path_ptr, &cwd) {
        Ok(path) => path,
        Err(errno) => {
            info.lock().errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    match remove_dir_recursive(&path) {
        Ok(_) => 0,
        Err(err) => {
            info.lock().errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_unlink(path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();

    info.lock().errno = Errno::Clear;

    let cwd = info.lock().cwd.lock().clone();
    let path = match read_user_path(path_ptr, &cwd) {
        Ok(path) => path,
        Err(errno) => {
            info.lock().errno = errno;
            return -1;
        }
    };

    interrupts::enable();

    match remove_file(&path) {
        Ok(_) => 0,
        Err(err) => {
            info.lock().errno = Errno::from(err);
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
    info.lock().errno = Errno::Clear;

    if buffer.is_null() {
        info.lock().errno = Errno::EFAULT;
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

        if !unsafe { try_copy_to_user(current_ptr, bytes.as_ptr(), bytes.len()) } {
            info.lock().errno = Errno::EFAULT;
            return -1;
        }
        written += bytes.len();
        current_ptr = unsafe { current_ptr.add(bytes.len()) };
    }

    written as i64
}

pub fn sys_list_mounts(buffer_ptr: *mut u8, buffer_size: usize) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    if buffer_size == 0 {
        return 0;
    }

    interrupts::enable();

    let mounts = list_mounts();
    let mut written = 0usize;
    let entry_size = core::mem::size_of::<RawMountEntry>();

    for mount in mounts {
        let path_str = mount.mount_point.to_string();
        let path_bytes = path_str.as_bytes();
        let total_size = entry_size + path_bytes.len();

        if written + total_size > buffer_size {
            break;
        }

        let entry = RawMountEntry {
            path_len: path_bytes.len() as u32,
            filesystem: filesystem_type_to_u32(mount.filesystem),
            device_id: mount.device_id as u64,
            partition_index: mount.partition_index as u64,
        };

        let entry_bytes = unsafe {
            core::slice::from_raw_parts((&entry as *const RawMountEntry).cast::<u8>(), entry_size)
        };

        if !unsafe { try_copy_to_user(buffer_ptr.add(written), entry_bytes.as_ptr(), entry_size) } {
            info.lock().errno = Errno::EFAULT;
            return -1;
        }
        written += entry_size;
        if !unsafe {
            try_copy_to_user(
                buffer_ptr.add(written),
                path_bytes.as_ptr(),
                path_bytes.len(),
            )
        } {
            info.lock().errno = Errno::EFAULT;
            return -1;
        }
        written += path_bytes.len();
    }

    written as i64
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct FstatEntry {
    pub size: u64,
    pub created: u64,
    pub accessed: u64,
    pub modified: u64,
    pub attrs: u16,
    pub kind: u8,
}

fn file_to_fstat_entry(file: &crate::fs::File) -> FstatEntry {
    let created = file
        .created
        .and_then(|ft| ft.to_datetime())
        .map(|dt| {
            // Convert to Unix timestamp approximation
            // This is a simplified conversion - real implementation would be more precise
            ((dt.year - 1970) as u64 * 365 * 24 * 3600)
                + (dt.month as u64 * 30 * 24 * 3600)
                + (dt.day as u64 * 24 * 3600)
                + (dt.hour as u64 * 3600)
                + (dt.min as u64 * 60)
                + (dt.sec as u64)
        })
        .unwrap_or(0);

    let accessed = file
        .accessed
        .and_then(|ft| ft.to_datetime())
        .map(|dt| {
            ((dt.year - 1970) as u64 * 365 * 24 * 3600)
                + (dt.month as u64 * 30 * 24 * 3600)
                + (dt.day as u64 * 24 * 3600)
                + (dt.hour as u64 * 3600)
                + (dt.min as u64 * 60)
                + (dt.sec as u64)
        })
        .unwrap_or(0);

    let modified = file
        .modified
        .and_then(|ft| ft.to_datetime())
        .map(|dt| {
            ((dt.year - 1970) as u64 * 365 * 24 * 3600)
                + (dt.month as u64 * 30 * 24 * 3600)
                + (dt.day as u64 * 24 * 3600)
                + (dt.hour as u64 * 3600)
                + (dt.min as u64 * 60)
                + (dt.sec as u64)
        })
        .unwrap_or(0);

    let mut attrs = 0u16;
    if file.attrs.readonly {
        attrs |= 1;
    }
    if file.attrs.hidden {
        attrs |= 2;
    }
    if file.attrs.system {
        attrs |= 4;
    }
    if file.attrs.archive {
        attrs |= 8;
    }

    let kind = match file.kind {
        crate::fs::FileKind::File => 0,
        crate::fs::FileKind::Directory => 1,
        crate::fs::FileKind::Symlink => 2,
        crate::fs::FileKind::Special => 3,
    };

    FstatEntry {
        size: file.size,
        created,
        accessed,
        modified,
        attrs,
        kind,
    }
}

pub fn sys_fstat(fd: u64, fstat_buf: *mut FstatEntry) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if fstat_buf.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let fd = info.lock().fd_table.lock().get_fd(fd).cloned();
    let fd_descriptor = match fd {
        Some(desc) => desc,
        None => {
            info.lock().errno = Errno::EBADF;
            return -1;
        }
    };

    let fstat_entry = match fd_descriptor {
        FileDescriptor::FsFile(fs_file) => {
            interrupts::enable();

            match file_info(&fs_file.path) {
                Ok(file) => file_to_fstat_entry(&file),
                Err(err) => {
                    info.lock().errno = Errno::from(err);
                    return -1;
                }
            }
        }
        FileDescriptor::StandardStream(_) => {
            FstatEntry {
                size: 0,
                created: 0,
                accessed: 0,
                modified: 0,
                attrs: 0,
                kind: 3, // Special file
            }
        }
        FileDescriptor::Pipe(_) => {
            FstatEntry {
                size: 0,
                created: 0,
                accessed: 0,
                modified: 0,
                attrs: 0,
                kind: 3, // Special file
            }
        }
    };

    if !unsafe { try_write_user(fstat_buf, fstat_entry) } {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    0
}

pub fn sys_stat(path_ptr: *const u8, path_len: usize, fstat_buf: *mut FstatEntry) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if fstat_buf.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let cwd = info.lock().cwd.lock().clone();

    let path = match read_user_path_with_len(path_ptr, path_len, &cwd) {
        Ok(p) => p,
        Err(err) => {
            info.lock().errno = err;
            return -1;
        }
    };

    interrupts::enable();

    let fstat_entry = match file_info(&path) {
        Ok(file) => file_to_fstat_entry(&file),
        Err(err) => {
            info.lock().errno = Errno::from(err);
            return -1;
        }
    };

    if !unsafe { try_write_user(fstat_buf, fstat_entry) } {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    0
}
