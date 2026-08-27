//! Device filesystem for exposing kernel devices to userspace.

use crate::debug::lock_order::RANK_DEVFS_REGISTRY;
use crate::thread::preempt::{PreemptRwLock, PreemptSpinlock};
use crate::{ranked_read, ranked_write};
use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use spin::Once;
use thiserror::Error;

use crate::{
    fs::{self, File, FileAttrs, FileKind, FileSystem, MmapRegion, handle::Pollable, path::Path},
    memory::mapper::MemoryManager,
    println,
};

pub mod block;

#[derive(Debug, Error, Clone)]
pub enum DevFsError {
    #[error("path already registered")]
    AlreadyExists,
    #[error("path not found")]
    NotFound,
    #[error("invalid path")]
    InvalidPath,
    #[error("operation unsupported")]
    Unsupported,
    #[error("i/o error")]
    IoError,
    #[error("device or resource busy")]
    Busy,
    #[error("no space left on device")]
    NoSpace,
}

impl From<DevFsError> for fs::Error {
    fn from(value: DevFsError) -> Self {
        match value {
            DevFsError::NotFound => fs::Error::FileNotFound,
            DevFsError::Busy => fs::Error::Busy,
            DevFsError::InvalidPath => fs::Error::InvalidArgument,
            DevFsError::NoSpace => fs::Error::NoSpace,
            DevFsError::AlreadyExists => fs::Error::AlreadyExists,
            DevFsError::Unsupported => fs::Error::Unsupported,
            DevFsError::IoError => fs::Error::IoError,
        }
    }
}

/// Trait implemented by kernel devices exposed through devfs.
///
/// Every method defaults to the refusal, so a device implements the operations
/// it has and says nothing about the rest. An override that returns
/// `Unsupported` is the default written out again and only hides which
/// operations the device really answers.
pub trait DevFsDevice: Send + Sync {
    fn read(&self, _offset: usize, _count: usize) -> Result<Vec<u8>, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    /// Read into a user buffer directly, where the device can do better than
    /// building a `Vec` for the caller to copy out of.
    ///
    /// The default keeps every device that has nothing to gain on the plain
    /// `read` path; only a device whose data is already in kernel pages, like a
    /// block device backed by the block page cache, is worth overriding for.
    fn read_to_user(
        &self,
        offset: usize,
        count: usize,
        user_ptr: *mut u8,
    ) -> Result<usize, DevFsError> {
        let data = self.read(offset, count)?;
        // SAFETY: `user_ptr` is the caller's buffer, checked by the syscall
        // layer, and the copy is bounded by what `read` produced.
        if !unsafe { crate::util::uaccess::try_copy_to_user(user_ptr, data.as_ptr(), data.len()) } {
            return Err(DevFsError::IoError);
        }
        Ok(data.len())
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    /// Device control.
    ///
    /// `arg` is either a scalar the request encodes its operands into, or a
    /// pointer to a kernel-owned buffer of **exactly `arg_len` bytes**, aligned
    /// to 8, holding a copy of what the caller passed. `arg_len` is zero in the
    /// scalar case.
    ///
    /// The length is the only thing bounding that buffer: userspace chooses it,
    /// so an implementation reading a header out of it must check
    /// `arg_len >= size_of::<Header>()` first, and one reading a variable-length
    /// tail must check the header's own count against the bytes that are left.
    /// Neither the syscall layer nor devfs can do it, since only the device
    /// knows the shape the request names.
    fn ioctl(&self, _request: u64, _arg: u64, _arg_len: usize) -> Result<u64, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn poll(&self) -> Result<Box<dyn Pollable>, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn mmap(
        &self,
        _offset: usize,
        _length: usize,
        _memory: Arc<PreemptSpinlock<MemoryManager>>,
    ) -> Result<MmapRegion, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn size(&self) -> u64 {
        0
    }
}

#[derive(Clone)]
struct DeviceNode {
    name: String,
    device: Arc<dyn DevFsDevice>,
}

impl DeviceNode {
    fn new(name: String, device: Arc<dyn DevFsDevice>) -> Self {
        Self { name, device }
    }

    fn file_entry(&self) -> File {
        File {
            name: self.name.clone(),
            kind: FileKind::Special,
            size: self.device.size(),
            attrs: FileAttrs {
                readonly: false,
                hidden: false,
                system: false,
                archive: false,
            },
            created: None,
            accessed: None,
            modified: None,
        }
    }
}

pub struct DevFs {
    directories: BTreeSet<Path>,
    devices: BTreeMap<Path, DeviceNode>,
}

impl DevFs {
    fn new() -> Self {
        let mut directories = BTreeSet::new();
        directories.insert(root_path());
        Self {
            directories,
            devices: BTreeMap::new(),
        }
    }

    fn ensure_directory(&mut self, path: &Path) {
        let mut current = root_path();
        self.directories.insert(current.clone());
        for component in path.components() {
            current = current.join(component);
            let normalized = current.normalize();
            self.directories.insert(normalized.clone());
            current = normalized;
        }
    }

    fn is_directory(&self, path: &Path) -> bool {
        self.directories.contains(path)
    }

    fn get_device(&self, path: &Path) -> Option<&DeviceNode> {
        self.devices.get(path)
    }

    fn insert_device(
        &mut self,
        path: Path,
        device: Arc<dyn DevFsDevice>,
    ) -> Result<(), DevFsError> {
        if path.is_root() {
            return Err(DevFsError::InvalidPath);
        }

        if self.devices.contains_key(&path) || self.directories.contains(&path) {
            return Err(DevFsError::AlreadyExists);
        }

        let parent = path.parent().unwrap_or_else(root_path);
        self.ensure_directory(&parent);

        let name = path.filename();
        let node = DeviceNode::new(name, device);
        self.devices.insert(path, node);
        Ok(())
    }

    fn remove_device(&mut self, path: &Path) -> Result<(), DevFsError> {
        if self.devices.remove(path).is_some() {
            Ok(())
        } else {
            Err(DevFsError::NotFound)
        }
    }
}

static DEVFS_INSTANCE: Once<Arc<PreemptRwLock<DevFs>>> = Once::new();

fn global_devfs() -> Arc<PreemptRwLock<DevFs>> {
    DEVFS_INSTANCE
        .call_once(|| Arc::new(PreemptRwLock::new(DevFs::new())))
        .clone()
}

fn root_path() -> Path {
    Path::parse("/").expect("root path").normalize()
}

pub struct DevFsHandle {
    shared: Arc<PreemptRwLock<DevFs>>,
}

impl DevFsHandle {
    pub fn new() -> Result<Self, fs::Error> {
        Ok(Self {
            shared: global_devfs(),
        })
    }
}

impl FileSystem for DevFsHandle {
    fn list_files(&self, path: &Path) -> Result<Vec<File>, fs::Error> {
        let normalized = path.normalize();

        // Snapshot the matching nodes under the guard, then build entries
        // outside it: `DeviceNode::file_entry` calls `DevFsDevice::size`, which
        // is a driver callback and may take that device's own lock.
        let (mut entries, nodes) = {
            let state = ranked_read!(RANK_DEVFS_REGISTRY, "devfs::list_files", self.shared);

            if !state.is_directory(&normalized) {
                return Err(fs::Error::NotADir);
            }

            let mut entries = Vec::new();
            for directory in state.directories.iter() {
                if normalized.is_direct_parent(directory) {
                    let mut name = directory.filename();
                    if name.is_empty() {
                        name = "/".to_string();
                    }
                    entries.push(File::synthetic_dir(name));
                }
            }

            let nodes: Vec<(String, DeviceNode)> = state
                .devices
                .iter()
                .filter(|(device_path, _)| normalized.is_direct_parent(device_path))
                .map(|(device_path, node)| (device_path.filename(), node.clone()))
                .collect();

            (entries, nodes)
        };

        for (name, node) in nodes {
            let mut entry = node.file_entry();
            entry.name = name;
            entries.push(entry);
        }

        Ok(entries)
    }

    fn read_bytes(&self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, fs::Error> {
        let normalized = path.normalize();
        let state = ranked_read!(RANK_DEVFS_REGISTRY, "devfs::read_bytes", self.shared);
        let device = state.get_device(&normalized).map(|d| d.device.clone());
        let is_dir = state.is_directory(&normalized);
        drop(state);

        if let Some(device) = device {
            device.read(offset, count).map_err(fs::Error::from)
        } else if is_dir {
            Err(fs::Error::NotAFile)
        } else {
            Err(fs::Error::FileNotFound)
        }
    }

    fn read_bytes_to_user(
        &self,
        path: &Path,
        offset: usize,
        count: usize,
        user_ptr: *mut u8,
    ) -> Result<usize, fs::Error> {
        let normalized = path.normalize();
        let state = ranked_read!(
            RANK_DEVFS_REGISTRY,
            "devfs::read_bytes_to_user",
            self.shared
        );
        let device = state.get_device(&normalized).map(|d| d.device.clone());
        let is_dir = state.is_directory(&normalized);
        // The registry lock is released before the copy: a user copy can demand
        // fault and park, and this lock is on the path every device open takes.
        drop(state);

        if let Some(device) = device {
            device
                .read_to_user(offset, count, user_ptr)
                .map_err(fs::Error::from)
        } else if is_dir {
            Err(fs::Error::NotAFile)
        } else {
            Err(fs::Error::FileNotFound)
        }
    }

    fn write_bytes(&self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, fs::Error> {
        let normalized = path.normalize();
        let state = ranked_read!(RANK_DEVFS_REGISTRY, "devfs::write_bytes", self.shared);
        let device = state.get_device(&normalized).map(|d| d.device.clone());
        let is_dir = state.is_directory(&normalized);
        drop(state);

        if let Some(device) = device {
            device
                .write(offset, data)
                .map(|written| written as u64)
                .map_err(fs::Error::from)
        } else if is_dir {
            Err(fs::Error::NotAFile)
        } else {
            Err(fs::Error::FileNotFound)
        }
    }

    fn create_file(&self, _path: &Path) -> Result<(), fs::Error> {
        Err(fs::Error::Unsupported)
    }

    fn create_dir(&self, _path: &Path) -> Result<(), fs::Error> {
        Err(fs::Error::Unsupported)
    }

    fn remove_dir(&self, _path: &Path) -> Result<(), fs::Error> {
        Err(fs::Error::Unsupported)
    }

    fn remove_file(&self, path: &Path) -> Result<(), fs::Error> {
        let normalized = path.normalize();
        let mut state = ranked_write!(RANK_DEVFS_REGISTRY, "devfs::remove_file", self.shared);
        state.remove_device(&normalized).map_err(fs::Error::from)
    }

    fn file_info(&self, path: &Path) -> Result<File, fs::Error> {
        let normalized = path.normalize();
        // Same reason as `list_files`: `file_entry` reaches a driver callback,
        // so the node is cloned out and the guard released first.
        let (node, is_dir) = {
            let state = ranked_read!(RANK_DEVFS_REGISTRY, "devfs::file_info", self.shared);
            (
                state.get_device(&normalized).cloned(),
                state.is_directory(&normalized),
            )
        };

        if let Some(device) = node {
            let mut entry = device.file_entry();
            entry.name = normalized.filename();
            Ok(entry)
        } else if is_dir {
            let mut name = normalized.filename();
            if name.is_empty() {
                name = "/".to_string();
            }
            Ok(File::synthetic_dir(name))
        } else {
            Err(fs::Error::FileNotFound)
        }
    }

    fn flush(&self) -> Result<(), fs::Error> {
        Ok(())
    }

    fn statfs(&self) -> Result<fs::StatFs, fs::Error> {
        let state = ranked_read!(RANK_DEVFS_REGISTRY, "devfs::statfs", self.shared);
        let devices = state.devices.len() as u64;
        let dirs = state.directories.len() as u64;
        let mut volume_name = [0u8; 64];
        volume_name[..4].copy_from_slice(b"dev\0");
        Ok(fs::StatFs {
            fs_type: "devfs",
            block_size: 0,
            total_blocks: 0,
            free_blocks: 0,
            total_inodes: devices + dirs,
            free_inodes: 0,
            volume_name,
            version: 0,
            block_groups: 0,
        })
    }

    fn ioctl(&self, path: &Path, request: u64, arg: u64, arg_len: usize) -> Result<u64, fs::Error> {
        let normalized = path.normalize();
        let state = ranked_read!(RANK_DEVFS_REGISTRY, "devfs::ioctl", self.shared);
        let device = state.get_device(&normalized).map(|d| d.device.clone());
        drop(state);

        if let Some(device) = device {
            device.ioctl(request, arg, arg_len).map_err(fs::Error::from)
        } else {
            Err(fs::Error::FileNotFound)
        }
    }

    fn poll(&self, path: &Path) -> Result<Box<dyn Pollable>, fs::Error> {
        let normalized = path.normalize();
        let state = ranked_read!(RANK_DEVFS_REGISTRY, "devfs::poll", self.shared);
        let device = state.get_device(&normalized).map(|d| d.device.clone());
        drop(state);

        if let Some(device) = device {
            device.poll().map_err(fs::Error::from)
        } else {
            Err(fs::Error::FileNotFound)
        }
    }

    fn mmap(
        &self,
        path: &Path,
        offset: usize,
        length: usize,
        memory: Arc<PreemptSpinlock<MemoryManager>>,
    ) -> Result<MmapRegion, fs::Error> {
        let normalized = path.normalize();
        let state = ranked_read!(RANK_DEVFS_REGISTRY, "devfs::mmap", self.shared);
        let device = state.get_device(&normalized).map(|d| d.device.clone());
        drop(state);

        if let Some(device) = device {
            device.mmap(offset, length, memory).map_err(fs::Error::from)
        } else {
            Err(fs::Error::FileNotFound)
        }
    }
}

/// Register a new device node within devfs.
pub fn register_device(path: &Path, device: Arc<dyn DevFsDevice>) -> Result<(), DevFsError> {
    println!("Registering device {path}");
    let normalized = path.normalize();
    let devfs = global_devfs();
    let mut devfs = ranked_write!(RANK_DEVFS_REGISTRY, "devfs::register_device", devfs);
    devfs.insert_device(normalized, device)
}

/// Convenience helper that parses a string path before registering a device.
pub fn register_device_str(path: &str, device: Arc<dyn DevFsDevice>) -> Result<(), DevFsError> {
    let path = Path::parse(path).map_err(|_| DevFsError::InvalidPath)?;
    register_device(&path, device)
}

/// Look up a device by its devfs-relative path (e.g. "/fb", "/mouse").
/// Returns a cloned Arc to the device if found.
/// This allows callers to bypass the FS Mailbox for devfs operations.
pub fn lookup_device(path: &Path) -> Option<Arc<dyn DevFsDevice>> {
    let devfs = DEVFS_INSTANCE.get()?;
    let devfs = ranked_read!(RANK_DEVFS_REGISTRY, "devfs::lookup_device", devfs);
    let normalized = path.normalize();
    devfs.get_device(&normalized).map(|d| d.device.clone())
}

/// Try to look up a device from a full VFS path (e.g. "/dev/fb").
/// Returns None if the path doesn't start with "/dev/" or the device doesn't exist.
pub fn try_lookup_from_full_path(full_path: &Path) -> Option<Arc<dyn DevFsDevice>> {
    let dev_prefix = Path::parse("/dev").ok()?;
    if !full_path.starts_with(&dev_prefix) {
        return None;
    }
    let relative = full_path.strip_prefix(&dev_prefix);
    lookup_device(&relative)
}

/// Remove an existing device node if present.
pub fn unregister_device(path: &Path) -> Result<(), DevFsError> {
    let normalized = path.normalize();
    let devfs = global_devfs();
    let mut devfs = ranked_write!(RANK_DEVFS_REGISTRY, "devfs::unregister_device", devfs);
    devfs.remove_device(&normalized)
}

/// Convenience helper for unregistering devices via string paths.
#[expect(
    unused,
    reason = "unregistering by path string; every caller holds the node instead"
)]
pub fn unregister_device_str(path: &str) -> Result<(), DevFsError> {
    let path = Path::parse(path).map_err(|_| DevFsError::InvalidPath)?;
    unregister_device(&path)
}
