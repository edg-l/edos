//! What is mounted, and how full it is.
//!
//! Two syscalls with packed reply buffers, decoded once here rather than in
//! every program that wants to know: the mount table is a run of fixed headers
//! each followed by a variable-length path, and `statfs` answers with a struct
//! whose two string fields are NUL-padded rather than terminated.

use crate::sys::{self, SYS_LIST_MOUNTS, SYS_STATFS, syscall2, syscall3};

/// Which filesystem is behind a mount.
///
/// The codes are the kernel's `filesystem_type_to_u32` and are part of the
/// syscall ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filesystem {
    Unknown,
    Fat12,
    Fat16,
    Fat32,
    Ntfs,
    Iso9660,
    Memfs,
    Devfs,
    Procfs,
    Efs,
}

impl Filesystem {
    fn from_code(code: u32) -> Self {
        match code {
            0 => Self::Unknown,
            1 => Self::Fat12,
            2 => Self::Fat16,
            3 => Self::Fat32,
            4 => Self::Ntfs,
            5 => Self::Iso9660,
            6 => Self::Memfs,
            7 => Self::Devfs,
            8 => Self::Procfs,
            9 => Self::Efs,
            other => unreachable!("kernel reported filesystem code {other}"),
        }
    }

    /// The name this filesystem is known by, as `mount` and `df` print it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Fat12 => "fat12",
            Self::Fat16 => "fat16",
            Self::Fat32 => "fat32",
            Self::Ntfs => "ntfs",
            Self::Iso9660 => "iso9660",
            Self::Memfs => "memfs",
            Self::Devfs => "devfs",
            Self::Procfs => "procfs",
            Self::Efs => "efs",
        }
    }

    /// Whether what is written here survives a reboot.
    ///
    /// False for the three that are made up on the spot: `/tmp` is RAM, and
    /// `/dev` and `/proc` are the kernel answering questions.
    pub fn is_persistent(self) -> bool {
        !matches!(self, Self::Memfs | Self::Devfs | Self::Procfs)
    }

    /// A one-line description of what standing on this filesystem means.
    pub fn nature(self) -> &'static str {
        match self {
            Self::Memfs => "in memory, forgotten on reboot",
            Self::Devfs => "device nodes",
            Self::Procfs => "kernel state, generated on read",
            Self::Unknown => "unrecognised",
            _ => "on disk",
        }
    }

    /// What to call the device behind the mount. The synthetic filesystems have
    /// no device, so they name themselves.
    pub fn device_label(self, device_id: u64, partition: u64) -> String {
        match self {
            Self::Memfs | Self::Devfs | Self::Procfs => self.name().to_string(),
            _ => format!("dev{device_id}p{partition}"),
        }
    }
}

/// One mounted filesystem.
pub struct Mount {
    pub path: String,
    pub filesystem: Filesystem,
    pub device_id: u64,
    pub partition: u64,
}

/// Size of the fixed part of a mount entry: path_len, filesystem, device_id,
/// partition_index. The path follows it, unterminated.
const MOUNT_HEADER: usize = 24;

/// Every mounted filesystem, in the order the kernel holds them.
pub fn list() -> Vec<Mount> {
    let mut buf = vec![0u8; 4096];
    let written = unsafe { syscall2(SYS_LIST_MOUNTS, buf.as_mut_ptr() as u64, buf.len() as u64) };
    let written = written as i64;
    if written <= 0 {
        return Vec::new();
    }
    let len = (written as usize).min(buf.len());

    let mut mounts = Vec::new();
    let mut offset = 0;
    while offset + MOUNT_HEADER <= len {
        let word = |at: usize| u32::from_le_bytes(buf[at..at + 4].try_into().unwrap());
        let long = |at: usize| u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        let path_len = word(offset) as usize;
        let filesystem = Filesystem::from_code(word(offset + 4));
        let device_id = long(offset + 8);
        let partition = long(offset + 16);
        offset += MOUNT_HEADER;

        if offset + path_len > len {
            break;
        }
        let path = String::from_utf8_lossy(&buf[offset..offset + path_len]).into_owned();
        offset += path_len;

        // The root mount reports an empty path.
        let path = if path.is_empty() {
            "/".to_string()
        } else {
            path
        };
        mounts.push(Mount {
            path,
            filesystem,
            device_id,
            partition,
        });
    }
    mounts
}

/// The mount a path is served by: the longest mount point that prefixes it.
pub fn covering<'a>(mounts: &'a [Mount], path: &str) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|mount| covers(&mount.path, path))
        .max_by_key(|mount| mount.path.len())
}

/// Whether `mount_point` is `path` or one of its ancestors, comparing whole
/// components so `/var` does not claim `/vararg`.
fn covers(mount_point: &str, path: &str) -> bool {
    if mount_point == "/" {
        return path.starts_with('/');
    }
    match path.strip_prefix(mount_point) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawStatFs {
    fs_type: [u8; 16],
    block_size: u64,
    total_blocks: u64,
    free_blocks: u64,
    total_inodes: u64,
    free_inodes: u64,
    volume_name: [u8; 64],
    version: u32,
    block_groups: u16,
    _pad: [u8; 2],
}

/// How much room a filesystem has, and what it calls itself.
pub struct StatFs {
    pub fs_type: String,
    pub volume_name: String,
    pub block_size: u64,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
    pub version: u32,
    pub block_groups: u16,
}

impl StatFs {
    pub fn total_bytes(&self) -> u64 {
        self.total_blocks * self.block_size
    }

    pub fn free_bytes(&self) -> u64 {
        self.free_blocks * self.block_size
    }

    pub fn used_bytes(&self) -> u64 {
        self.total_bytes().saturating_sub(self.free_bytes())
    }

    /// Percentage of the filesystem in use, or None when it reports no size at
    /// all, which is what the synthetic filesystems do.
    pub fn used_percent(&self) -> Option<u32> {
        let total = self.total_bytes();
        if total == 0 {
            return None;
        }
        Some(((self.used_bytes() as u128 * 100) / total as u128) as u32)
    }
}

/// Ask a filesystem about itself, through any path it serves.
pub fn statfs(path: &str) -> Option<StatFs> {
    let path_c = format!("{path}\0");
    let mut buf = [0u8; core::mem::size_of::<RawStatFs>()];
    let ret = unsafe {
        syscall3(
            SYS_STATFS,
            path_c.as_ptr() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
    if sys::is_err(ret) {
        return None;
    }

    let raw = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const RawStatFs) };
    Some(StatFs {
        fs_type: padded_str(&raw.fs_type),
        volume_name: padded_str(&raw.volume_name),
        block_size: raw.block_size,
        total_blocks: raw.total_blocks,
        free_blocks: raw.free_blocks,
        total_inodes: raw.total_inodes,
        free_inodes: raw.free_inodes,
        version: raw.version,
        block_groups: raw.block_groups,
    })
}

/// Read a NUL-padded fixed-width field.
fn padded_str(field: &[u8]) -> String {
    let len = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..len]).into_owned()
}
