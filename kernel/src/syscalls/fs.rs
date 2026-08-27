use alloc::{ffi::CString, string::ToString};
use bytemuck::{Pod, Zeroable};
use syscall_abi::{RawStatFs, Stat};
use x86_64::instructions::interrupts;

use crate::{
    fs::{
        Error, FileKind,
        api::{
            create_dir, file_info, file_info_nofollow, list_files, list_mounts, list_partitions,
            mount_partition, remove_dir, remove_file,
        },
        gpt::FilesystemType,
        path::Path,
        vfs,
    },
    syscalls::io::{current_cwd, resolve_path},
    thread::pipe::FileDescriptor,
    util::uaccess::{
        UAccessError, try_copy_from_user, try_copy_string_from_user, try_copy_to_user,
        try_write_user,
    },
};

use super::{Errno, MAX_PATH_LEN, PathBuf, copy_user_path, copy_user_path_len};
use crate::thread::scheduler::current_thread_info;

fn read_user_path(path_ptr: *const u8, cwd: &Path) -> Result<Path, Errno> {
    if path_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let mut buf: PathBuf = [0u8; MAX_PATH_LEN];
    let path_str = copy_user_path(&mut buf, path_ptr)?;
    resolve_path(path_str, cwd).map_err(|_| Errno::EINVAL)
}

pub(super) fn read_user_path_with_len(
    path_ptr: *const u8,
    path_len: usize,
    cwd: &Path,
) -> Result<Path, Errno> {
    let mut buf: PathBuf = [0u8; MAX_PATH_LEN];
    let path_str = copy_user_path_len(&mut buf, path_ptr, path_len)?;
    resolve_path(path_str, cwd).map_err(|_| Errno::EINVAL)
}

/// `dirfd` naming the calling process's working directory.
pub(super) const AT_FDCWD: i64 = -100;

/// Resolve a user path the way the `*at` family does: an absolute path ignores
/// `dirfd`, `AT_FDCWD` resolves against the working directory, and any other
/// value must be a descriptor open on a directory.
///
/// Enables interrupts before checking that a descriptor names a directory,
/// since that check walks the filesystem. Every caller enables them for the
/// operation itself anyway.
pub(super) fn read_user_path_at(
    dirfd: i64,
    path_ptr: *const u8,
    path_len: usize,
) -> Result<Path, Errno> {
    let mut buf: PathBuf = [0u8; MAX_PATH_LEN];
    let path_str = copy_user_path_len(&mut buf, path_ptr, path_len)?;

    if path_str.starts_with('/') || dirfd == AT_FDCWD {
        let info = current_thread_info();
        let cwd = current_cwd(&info);
        return resolve_path(path_str, &cwd).map_err(|_| Errno::EINVAL);
    }

    let base = at_dir_path(dirfd)?;
    Ok(base.join(path_str).normalize())
}

/// The path a descriptor was opened by, whatever kind of file it names.
fn fd_path(fd: i64) -> Result<Path, Errno> {
    if fd < 0 {
        return Err(Errno::EBADF);
    }
    let info = current_thread_info();
    let fd_table = info.lock().fd_table.clone();
    let descriptor = fd_table.lock();
    match descriptor.get_fd(fd as u64) {
        Some(FileDescriptor::FsFile(file)) => Ok(file.path.clone()),
        Some(_) => Err(Errno::EINVAL),
        None => Err(Errno::EBADF),
    }
}

/// The directory a `*at` descriptor names.
fn at_dir_path(dirfd: i64) -> Result<Path, Errno> {
    if dirfd < 0 {
        return Err(Errno::EBADF);
    }

    let info = current_thread_info();
    let fd_table = info.lock().fd_table.clone();
    let base = match fd_table.lock().get_fd(dirfd as u64) {
        Some(FileDescriptor::FsFile(file)) => file.path.clone(),
        Some(_) => return Err(Errno::ENOTDIR),
        None => return Err(Errno::EBADF),
    };

    interrupts::enable();

    match file_info(&base) {
        Ok(finfo) if finfo.kind == FileKind::Directory => Ok(base),
        Ok(_) => Err(Errno::ENOTDIR),
        Err(err) => Err(Errno::from(err)),
    }
}

fn read_user_str(value_ptr: *const u8) -> Result<CString, Errno> {
    if value_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let mut buf: PathBuf = [0u8; MAX_PATH_LEN];
    // SAFETY: `buf` is a `MAX_PATH_LEN` array and that is the cap passed,
    // so the copy cannot run past its end.
    let len = match unsafe { try_copy_string_from_user(buf.as_mut_ptr(), value_ptr, MAX_PATH_LEN) }
    {
        Ok(len) => len,
        Err(UAccessError::TooLong) => return Err(Errno::EINVAL),
        Err(UAccessError::Fault) => return Err(Errno::EFAULT),
    };

    CString::new(&buf[..len]).map_err(|_| Errno::EINVAL)
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

/// Run `act` on a path the caller named relative to its working directory.
///
/// `mkdir`, `rmdir`, `rmdir_all` and `unlink` differ only in `act`. Everything
/// around it is the same in all four: clear `errno`, resolve the path, enable
/// interrupts before doing any filesystem work, and report the outcome the way
/// the ABI says. A copy of that body per syscall is four places for the
/// interrupt enable or the errno convention to drift.
fn on_cwd_path<T>(
    path_ptr: *const u8,
    act: impl FnOnce(&Path) -> Result<T, Error>,
) -> Result<u64, Errno> {
    let info = current_thread_info();
    let cwd = current_cwd(&info);
    let path = read_user_path(path_ptr, &cwd)?;

    interrupts::enable();

    act(&path).map_err(Errno::from)?;
    Ok(0)
}

/// As [`on_cwd_path`], for the `*at` forms: the path is resolved against
/// `dirfd` and carries its own length rather than a terminator.
fn on_dir_path<T>(
    dirfd: i64,
    path_ptr: *const u8,
    path_len: usize,
    act: impl FnOnce(&Path) -> Result<T, Error>,
) -> Result<u64, Errno> {
    let path = read_user_path_at(dirfd, path_ptr, path_len)?;

    interrupts::enable();

    act(&path).map_err(Errno::from)?;
    Ok(0)
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
        FilesystemType::Efs => 9,
    }
}

pub fn sys_mount(
    device_id: u64,
    partition_idx: u64,
    path_ptr: *const u8,
    fs_type: *const u8,
) -> Result<u64, Errno> {
    let info = current_thread_info();
    let cwd = current_cwd(&info);
    let mount_point = read_user_path(path_ptr, &cwd)?;

    interrupts::enable();

    let finfo = match file_info(&mount_point) {
        Ok(info) => info,
        Err(Error::FileNotFound) => {
            return Err(Errno::ENOENT);
        }
        Err(err) => {
            return Err(Errno::from(err));
        }
    };

    if finfo.kind != FileKind::Directory {
        return Err(Errno::ENOTDIR);
    }

    let fs_type = read_user_str(fs_type)?.to_string_lossy().to_string();

    let fs_type = match fs_type.as_str() {
        "fat32" => FilesystemType::Fat32,
        "efs" => FilesystemType::Efs,
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
        Ok(_) => Ok(0),
        Err(err) => Err(Errno::from(err)),
    }
}

pub fn sys_mkdir(path_ptr: *const u8) -> Result<u64, Errno> {
    on_cwd_path(path_ptr, create_dir)
}

pub fn sys_rmdir(path_ptr: *const u8) -> Result<u64, Errno> {
    on_cwd_path(path_ptr, remove_dir)
}

pub fn sys_rmdir_all(path_ptr: *const u8) -> Result<u64, Errno> {
    on_cwd_path(path_ptr, remove_dir_recursive)
}

pub fn sys_unlink(path_ptr: *const u8) -> Result<u64, Errno> {
    on_cwd_path(path_ptr, remove_file)
}

/// `flags` bit selecting `rmdir` semantics, as in Linux `<fcntl.h>`.
const AT_REMOVEDIR: u64 = 0x200;

/// Create a named pipe relative to a directory descriptor.
///
/// Nothing opens it here: the buffer its ends meet in comes into being when
/// the first `open` arrives, so this only puts the name and its type into the
/// filesystem.
pub fn sys_mkfifoat(dirfd: i64, path_ptr: *const u8, path_len: usize) -> Result<u64, Errno> {
    on_dir_path(dirfd, path_ptr, path_len, crate::fs::api::create_fifo)
}

/// Create a directory relative to a directory descriptor.
///
/// No `mode` argument: EDOS carries no permission bits, so one would be a
/// value nothing could observe.
pub fn sys_mkdirat(dirfd: i64, path_ptr: *const u8, path_len: usize) -> Result<u64, Errno> {
    on_dir_path(dirfd, path_ptr, path_len, create_dir)
}

/// Remove a file or directory relative to a directory descriptor.
///
/// `AT_REMOVEDIR` removes an empty directory instead of a file; no other flag
/// is defined.
pub fn sys_unlinkat(
    dirfd: i64,
    path_ptr: *const u8,
    path_len: usize,
    flags: u64,
) -> Result<u64, Errno> {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(Errno::EINVAL);
    }

    on_dir_path(dirfd, path_ptr, path_len, |path| {
        if flags & AT_REMOVEDIR != 0 {
            remove_dir(path)
        } else {
            remove_file(path)
        }
    })
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

pub fn sys_list_partitions(buffer: *mut u8, size: u64) -> Result<u64, Errno> {
    if buffer.is_null() {
        return Err(Errno::EFAULT);
    }

    interrupts::enable();

    let partitions = match list_partitions() {
        Ok(parts) => parts,
        Err(_) => {
            return Err(Errno::EIO);
        }
    };

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

        // SAFETY: `bytes` is `bytemuck::bytes_of` a live `SysPartition`, so the
        // length is its own; the loop breaks before `written` can exceed the
        // caller's `size`.
        if !unsafe { try_copy_to_user(current_ptr, bytes.as_ptr(), bytes.len()) } {
            return Err(Errno::EFAULT);
        }
        written += bytes.len();
        // SAFETY: the same bound -- `written + bytes.len()` was checked against
        // `size` above, so the walked pointer stays inside the caller's buffer.
        current_ptr = unsafe { current_ptr.add(bytes.len()) };
    }

    Ok(written as u64)
}

pub fn sys_list_mounts(buffer_ptr: *mut u8, buffer_size: usize) -> Result<u64, Errno> {
    if buffer_ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    if buffer_size == 0 {
        return Ok(0);
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

        // SAFETY: `entry` is a live `RawMountEntry` and `entry_size` is its own
        // `size_of`, so the slice covers its storage and nothing else.
        let entry_bytes = unsafe {
            core::slice::from_raw_parts((&entry as *const RawMountEntry).cast::<u8>(), entry_size)
        };

        // SAFETY: `written + total_size` was checked against `buffer_size`
        // above, so the offset and the `entry_size` behind it are both inside the
        // caller's buffer.
        if !unsafe { try_copy_to_user(buffer_ptr.add(written), entry_bytes.as_ptr(), entry_size) } {
            return Err(Errno::EFAULT);
        }
        written += entry_size;
        // SAFETY: the same check covered the name that follows the record.
        if !unsafe {
            try_copy_to_user(
                buffer_ptr.add(written),
                path_bytes.as_ptr(),
                path_bytes.len(),
            )
        } {
            return Err(Errno::EFAULT);
        }
        written += path_bytes.len();
    }

    Ok(written as u64)
}

fn file_to_fstat_entry(file: &crate::fs::File) -> Stat {
    let unix_secs = |time: Option<crate::fs::FileTime>| {
        time.and_then(|ft| ft.to_datetime())
            .map(|dt| dt.to_unix_secs())
            .unwrap_or(0)
    };

    let created = unix_secs(file.created);
    let accessed = unix_secs(file.accessed);
    let modified = unix_secs(file.modified);

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
        crate::fs::FileKind::Fifo => 4,
    };

    Stat {
        size: file.size,
        created,
        accessed,
        modified,
        attrs,
        kind,
    }
}

pub fn sys_fstat(fd: u64, fstat_buf: *mut Stat) -> Result<u64, Errno> {
    let info = current_thread_info();
    if fstat_buf.is_null() {
        return Err(Errno::EFAULT);
    }

    // The fd table is a `BlockingMutex` and its contended path parks, so the
    // `Arc` leaves the thread-info `IrqSpinlock` before it is locked: taken
    // inside that guard, the park would happen with interrupts disabled.
    let fd_table = info.lock().fd_table.clone();
    let fd = fd_table.lock().get_fd(fd).cloned();
    let fd_descriptor = match fd {
        Some(desc) => desc,
        None => {
            return Err(Errno::EBADF);
        }
    };

    let fstat_entry = match fd_descriptor {
        FileDescriptor::FsFile(fs_file) => {
            interrupts::enable();

            match file_info(&fs_file.path) {
                Ok(file) => file_to_fstat_entry(&file),
                Err(err) => {
                    return Err(Errno::from(err));
                }
            }
        }
        FileDescriptor::StandardStream(_) => {
            Stat {
                size: 0,
                created: 0,
                accessed: 0,
                modified: 0,
                attrs: 0,
                kind: 3, // Special file
            }
        }
        FileDescriptor::PipeRead(_)
        | FileDescriptor::PipeWrite(_)
        | FileDescriptor::PipeReadWrite(_) => {
            Stat {
                size: 0,
                created: 0,
                accessed: 0,
                modified: 0,
                attrs: 0,
                kind: 3, // Special file
            }
        }
        FileDescriptor::PtyMaster(_) | FileDescriptor::PtySlave(_) => {
            Stat {
                size: 0,
                created: 0,
                accessed: 0,
                modified: 0,
                attrs: 0,
                kind: 3, // Special file
            }
        }
        FileDescriptor::Socket(_) => {
            Stat {
                size: 0,
                created: 0,
                accessed: 0,
                modified: 0,
                attrs: 0,
                kind: 3, // Special file
            }
        }
    };

    // SAFETY: the value is a live local and `try_write_user` writes exactly
    // its `size_of` to the caller's pointer, which it range-checks.
    if !unsafe { try_write_user(fstat_buf, fstat_entry) } {
        return Err(Errno::EFAULT);
    }

    Ok(0)
}

pub fn sys_stat(path_ptr: *const u8, path_len: usize, fstat_buf: *mut Stat) -> Result<u64, Errno> {
    sys_fstatat(AT_FDCWD, path_ptr, path_len, fstat_buf, 0)
}

/// Report on a symbolic link itself rather than on what it names, as in POSIX
/// `<fcntl.h>`. This is what makes `lstat` distinguishable from `stat`.
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;

/// Stat a path relative to a directory descriptor.
///
/// `AT_SYMLINK_NOFOLLOW` is the only accepted flag; anything else is refused
/// rather than quietly ignored. With it, a symbolic link reports its own
/// `Symlink` kind and the length of its target, which is the only way a caller
/// can tell a link from the file it names without `readlink`.
pub fn sys_fstatat(
    dirfd: i64,
    path_ptr: *const u8,
    path_len: usize,
    fstat_buf: *mut Stat,
    flags: u64,
) -> Result<u64, Errno> {
    if fstat_buf.is_null() {
        return Err(Errno::EFAULT);
    }
    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return Err(Errno::EINVAL);
    }
    let nofollow = flags & AT_SYMLINK_NOFOLLOW != 0;

    let path = read_user_path_at(dirfd, path_ptr, path_len)?;

    interrupts::enable();

    let looked_up = if nofollow {
        file_info_nofollow(&path)
    } else {
        file_info(&path)
    };
    let fstat_entry = match looked_up {
        Ok(file) => file_to_fstat_entry(&file),
        Err(err) => {
            return Err(Errno::from(err));
        }
    };

    // SAFETY: as above -- a live local written by its own size.
    if !unsafe { try_write_user(fstat_buf, fstat_entry) } {
        return Err(Errno::EFAULT);
    }

    Ok(0)
}

/// Bits of the `mode` argument to `sys_access`, as in POSIX `<unistd.h>`.
const X_OK: u32 = 1;
const W_OK: u32 = 2;
const R_OK: u32 = 4;
const ACCESS_MODE_BITS: u32 = X_OK | W_OK | R_OK;

pub fn sys_access(path_ptr: *const u8, path_len: usize, mode: u32) -> Result<u64, Errno> {
    sys_faccessat(AT_FDCWD, path_ptr, path_len, mode, 0)
}

/// faccessat(dirfd, path, path_len, mode, flags) -> 0 if the access is
/// permitted, -1 otherwise
///
/// EDOS carries no per-file permission bits and every process runs with the
/// same credentials, so the answer is existence plus the read-only attribute:
/// `W_OK` on a read-only file is denied with EACCES, and `R_OK` and `X_OK` are
/// granted for anything that exists. `mode` of 0 (`F_OK`) is an existence test.
///
/// `flags` must be 0. `AT_EACCESS` asks about the effective ids, which are the
/// only ids there are here, and `file_info` follows symbolic links, so
/// `AT_SYMLINK_NOFOLLOW` cannot be honoured; both are refused rather than
/// quietly ignored.
pub fn sys_faccessat(
    dirfd: i64,
    path_ptr: *const u8,
    path_len: usize,
    mode: u32,
    flags: u64,
) -> Result<u64, Errno> {
    if mode & !ACCESS_MODE_BITS != 0 || flags != 0 {
        return Err(Errno::EINVAL);
    }

    let path = read_user_path_at(dirfd, path_ptr, path_len)?;

    interrupts::enable();

    let file = match file_info(&path) {
        Ok(file) => file,
        Err(err) => {
            return Err(Errno::from(err));
        }
    };

    if mode & W_OK != 0 && file.attrs.readonly {
        return Err(Errno::EACCES);
    }

    Ok(0)
}

/// Resize the file named by a path.
///
/// The path-based form of `ftruncate`, for the callers that have a name and no
/// descriptor. A directory is refused rather than resized.
pub fn sys_truncate(path_ptr: *const u8, path_len: usize, size: u64) -> Result<u64, Errno> {
    let info = current_thread_info();
    let cwd = current_cwd(&info);

    let path = read_user_path_with_len(path_ptr, path_len, &cwd)?;

    interrupts::enable();

    match file_info(&path) {
        Ok(finfo) if finfo.kind == FileKind::Directory => {
            return Err(Errno::EISDIR);
        }
        Ok(_) => {}
        Err(err) => {
            return Err(Errno::from(err));
        }
    }

    match crate::fs::api::truncate(&path, size) {
        Ok(()) => Ok(0),
        Err(err) => Err(Errno::from(err)),
    }
}

pub fn sys_symlink(
    target_ptr: *const u8,
    target_len: usize,
    path_ptr: *const u8,
    path_len: usize,
) -> Result<u64, Errno> {
    sys_symlinkat(target_ptr, target_len, AT_FDCWD, path_ptr, path_len)
}

/// Create a symbolic link at `path`, relative to the directory descriptor
/// `newdirfd`, holding `target`. The target is stored verbatim, so a relative
/// one resolves against the link's own directory and a dangling one is legal,
/// as in POSIX; `newdirfd` therefore names where the link goes, never what it
/// points at.
pub fn sys_symlinkat(
    target_ptr: *const u8,
    target_len: usize,
    newdirfd: i64,
    path_ptr: *const u8,
    path_len: usize,
) -> Result<u64, Errno> {
    if target_ptr.is_null() {
        return Err(Errno::EFAULT);
    }
    if target_len == 0 || target_len > MAX_PATH_LEN {
        return Err(Errno::EINVAL);
    }

    let mut target_buf: PathBuf = [0u8; MAX_PATH_LEN];
    // SAFETY: `target_len` was checked against `MAX_PATH_LEN`, which is
    // `target_buf`'s length, above.
    if !unsafe { try_copy_from_user(target_buf.as_mut_ptr(), target_ptr, target_len) } {
        return Err(Errno::EFAULT);
    }
    let target = match core::str::from_utf8(&target_buf[..target_len]) {
        Ok(s) => s,
        Err(_) => {
            return Err(Errno::EINVAL);
        }
    };

    let path = read_user_path_at(newdirfd, path_ptr, path_len)?;

    interrupts::enable();

    match crate::fs::api::symlink(target, &path) {
        Ok(()) => Ok(0),
        Err(err) => Err(Errno::from(err)),
    }
}

pub fn sys_readlink(
    path_ptr: *const u8,
    path_len: usize,
    buf: *mut u8,
    buf_len: usize,
) -> Result<u64, Errno> {
    sys_readlinkat(AT_FDCWD, path_ptr, path_len, buf, buf_len)
}

/// Copy the target of the symbolic link at `path`, relative to the directory
/// descriptor `dirfd`, into `buf` without a terminating NUL. Returns the number
/// of bytes written, which is `buf_len` when the target was truncated, as in
/// POSIX.
pub fn sys_readlinkat(
    dirfd: i64,
    path_ptr: *const u8,
    path_len: usize,
    buf: *mut u8,
    buf_len: usize,
) -> Result<u64, Errno> {
    if buf.is_null() {
        return Err(Errno::EFAULT);
    }

    let path = read_user_path_at(dirfd, path_ptr, path_len)?;

    interrupts::enable();

    let target = match crate::fs::api::read_link(&path) {
        Ok(t) => t,
        Err(err) => {
            return Err(Errno::from(err));
        }
    };

    let count = target.len().min(buf_len);
    // SAFETY: `count` is clamped to `target.len()`, so the source is valid
    // for it, and to `buf_len`, the caller's own claim about its buffer.
    if count > 0 && !unsafe { try_copy_to_user(buf, target.as_ptr(), count) } {
        return Err(Errno::EFAULT);
    }
    Ok(count as u64)
}

/// Rename between two directory descriptors.
///
/// Each path resolves against its own directory descriptor, so one rename can
/// name two of them.
pub fn sys_renameat(
    olddirfd: i64,
    old_ptr: *const u8,
    old_len: usize,
    newdirfd: i64,
    new_ptr: *const u8,
    new_len: usize,
) -> Result<u64, Errno> {
    let old_path = read_user_path_at(olddirfd, old_ptr, old_len)?;
    let new_path = read_user_path_at(newdirfd, new_ptr, new_len)?;

    rename_resolved(&old_path, &new_path)
}

/// Rename an already-resolved path, shared with `sys_rename`, which takes
/// NUL-terminated paths and so cannot go through [`read_user_path_at`].
pub(super) fn rename_resolved(old: &Path, new: &Path) -> Result<u64, Errno> {
    interrupts::enable();

    match crate::fs::api::rename(old, new) {
        Ok(()) => Ok(0),
        Err(err) => Err(Errno::from(err)),
    }
}

/// One half of `utimensat`'s `times` argument, as POSIX `struct timespec`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct UserTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// `tv_nsec` values that name a time instead of carrying one, as in Linux
/// `<sys/stat.h>`: take the current time, or leave the timestamp alone.
const UTIME_NOW: i64 = (1 << 30) - 1;
const UTIME_OMIT: i64 = (1 << 30) - 2;

/// Stamp a file's access and modification times.
///
/// `times` points at two `timespec`s, access then modification; a null pointer
/// stamps both with the current time. `UTIME_NOW` and `UTIME_OMIT` in `tv_nsec`
/// have their POSIX meanings. Timestamps are stored to whole seconds, so
/// `tv_nsec` is otherwise dropped.
pub fn sys_utimensat(
    dirfd: i64,
    path_ptr: *const u8,
    path_len: usize,
    times: *const UserTimespec,
    flags: u64,
) -> Result<u64, Errno> {
    // `set_times` resolves through symbolic links, so a request not to follow
    // one cannot be honoured and is refused rather than quietly ignored.
    if flags != 0 {
        return Err(Errno::EINVAL);
    }

    // No path means `dirfd` names the file itself, which is POSIX `futimens`
    // and the only way to set the times of a file a caller holds open: `std`'s
    // `File` carries a descriptor and never the name it was opened by.
    let resolved = if path_ptr.is_null() && path_len == 0 {
        fd_path(dirfd)
    } else {
        read_user_path_at(dirfd, path_ptr, path_len)
    };
    let path = resolved?;

    let (atime, mtime) = if times.is_null() {
        (Some(now_unix_secs()), Some(now_unix_secs()))
    } else {
        let mut pair = [UserTimespec {
            tv_sec: 0,
            tv_nsec: 0,
        }; 2];
        // SAFETY: `pair` is two `UserTimespec`, which is exactly the length
        // named.
        let copied = unsafe {
            try_copy_from_user(
                pair.as_mut_ptr() as *mut u8,
                times as *const u8,
                core::mem::size_of::<UserTimespec>() * 2,
            )
        };
        if !copied {
            return Err(Errno::EFAULT);
        }

        let resolve = |ts: UserTimespec| match ts.tv_nsec {
            UTIME_OMIT => Ok(None),
            UTIME_NOW => Ok(Some(now_unix_secs())),
            n if !(0..1_000_000_000).contains(&n) || ts.tv_sec < 0 => Err(Errno::EINVAL),
            _ => Ok(Some(ts.tv_sec as u64)),
        };

        match (resolve(pair[0]), resolve(pair[1])) {
            (Ok(a), Ok(m)) => (a, m),
            _ => {
                return Err(Errno::EINVAL);
            }
        }
    };

    if atime.is_none() && mtime.is_none() {
        return Ok(0);
    }

    interrupts::enable();

    match crate::fs::api::set_times(&path, atime, mtime) {
        Ok(()) => Ok(0),
        Err(err) => Err(Errno::from(err)),
    }
}

fn now_unix_secs() -> u64 {
    crate::fs::DateTime::now().to_unix_secs()
}

/// Report a mounted filesystem's geometry and free space.
pub fn sys_statfs(path_ptr: *const u8, buf: *mut u8, buf_len: usize) -> Result<u64, Errno> {
    let info = current_thread_info();
    let cwd = current_cwd(&info);
    let path = read_user_path(path_ptr, &cwd)?;

    interrupts::enable();

    let op = match vfs::resolve(&path) {
        Some(op) => op,
        None => {
            return Err(Errno::ENOENT);
        }
    };

    let stat = match vfs::statfs(&op) {
        Ok(s) => s,
        Err(_) => {
            return Err(Errno::EIO);
        }
    };

    let needed = core::mem::size_of::<RawStatFs>();
    if buf_len < needed {
        return Ok(needed as u64);
    }

    let mut raw = RawStatFs {
        fs_type: [0u8; 16],
        block_size: stat.block_size,
        total_blocks: stat.total_blocks,
        free_blocks: stat.free_blocks,
        total_inodes: stat.total_inodes,
        free_inodes: stat.free_inodes,
        volume_name: stat.volume_name,
        version: stat.version,
        block_groups: stat.block_groups,
        _pad: [0; 2],
    };

    let type_bytes = stat.fs_type.as_bytes();
    let copy_len = type_bytes.len().min(15);
    raw.fs_type[..copy_len].copy_from_slice(&type_bytes[..copy_len]);

    // SAFETY: the source is a live `RawStatFs` and `needed` is its
    // `size_of`, already compared against the caller's `buf_len` above.
    if !unsafe { try_copy_to_user(buf, &raw as *const RawStatFs as *const u8, needed) } {
        return Err(Errno::EFAULT);
    }

    Ok(0)
}
