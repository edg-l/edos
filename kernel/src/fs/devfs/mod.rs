//! Device filesystem for exposing kernel devices to userspace.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use spin::{Once, RwLock};
use thiserror::Error;

use crate::fs::{self, File, FileAttrs, FileKind, FileSystem, MmapRegion, PollState, path::Path};

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
}

impl From<DevFsError> for fs::Error {
    fn from(value: DevFsError) -> Self {
        match value {
            DevFsError::NotFound => fs::Error::FileNotFound,
            DevFsError::AlreadyExists
            | DevFsError::InvalidPath
            | DevFsError::Unsupported
            | DevFsError::IoError => fs::Error::IoError,
        }
    }
}

/// Trait implemented by kernel devices exposed through devfs.
pub trait DevFsDevice: Send + Sync {
    fn read(&self, _offset: usize, _count: usize) -> Result<Vec<u8>, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn ioctl(&self, _request: u64, _arg: u64) -> Result<u64, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn poll(&self) -> Result<PollState, DevFsError> {
        Err(DevFsError::Unsupported)
    }

    fn mmap(&self, _offset: usize, _length: usize) -> Result<MmapRegion, DevFsError> {
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

static DEVFS_INSTANCE: Once<Arc<RwLock<DevFs>>> = Once::new();

fn global_devfs() -> Arc<RwLock<DevFs>> {
    DEVFS_INSTANCE
        .call_once(|| Arc::new(RwLock::new(DevFs::new())))
        .clone()
}

fn root_path() -> Path {
    Path::parse("/").expect("root path").normalize()
}

pub struct DevFsHandle {
    shared: Arc<RwLock<DevFs>>,
}

impl DevFsHandle {
    pub fn new() -> Result<Self, fs::Error> {
        Ok(Self {
            shared: global_devfs(),
        })
    }
}

impl FileSystem for DevFsHandle {
    fn list_files(&mut self, path: &Path) -> Result<Vec<File>, fs::Error> {
        let normalized = path.normalize();
        let state = self.shared.read();

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
                entries.push(File {
                    name,
                    kind: FileKind::Directory,
                    size: 0,
                    attrs: FileAttrs {
                        readonly: false,
                        hidden: false,
                        system: false,
                        archive: false,
                    },
                    created: None,
                    accessed: None,
                    modified: None,
                });
            }
        }

        for (device_path, device) in state.devices.iter() {
            if normalized.is_direct_parent(device_path) {
                let mut entry = device.file_entry();
                entry.name = device_path.filename();
                entries.push(entry);
            }
        }

        Ok(entries)
    }

    fn read_bytes(
        &mut self,
        path: &Path,
        offset: usize,
        count: usize,
    ) -> Result<Vec<u8>, fs::Error> {
        let normalized = path.normalize();
        let state = self.shared.read();
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

    fn write_bytes(&mut self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, fs::Error> {
        let normalized = path.normalize();
        let state = self.shared.read();
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

    fn create_file(&mut self, _path: &Path) -> Result<(), fs::Error> {
        Err(fs::Error::IoError)
    }

    fn create_dir(&mut self, _path: &Path) -> Result<(), fs::Error> {
        Err(fs::Error::IoError)
    }

    fn remove_dir(&mut self, _path: &Path) -> Result<(), fs::Error> {
        Err(fs::Error::IoError)
    }

    fn remove_file(&mut self, path: &Path) -> Result<(), fs::Error> {
        let normalized = path.normalize();
        let mut state = self.shared.write();
        state.remove_device(&normalized).map_err(fs::Error::from)
    }

    fn file_info(&mut self, path: &Path) -> Result<File, fs::Error> {
        let normalized = path.normalize();
        let state = self.shared.read();

        if let Some(device) = state.get_device(&normalized) {
            let mut entry = device.file_entry();
            entry.name = normalized.filename();
            Ok(entry)
        } else if state.is_directory(&normalized) {
            let mut name = normalized.filename();
            if name.is_empty() {
                name = "/".to_string();
            }
            Ok(File {
                name,
                kind: FileKind::Directory,
                size: 0,
                attrs: FileAttrs {
                    readonly: false,
                    hidden: false,
                    system: false,
                    archive: false,
                },
                created: None,
                accessed: None,
                modified: None,
            })
        } else {
            Err(fs::Error::FileNotFound)
        }
    }

    fn flush(&mut self) -> Result<(), fs::Error> {
        Ok(())
    }

    fn ioctl(&mut self, path: &Path, request: u64, arg: u64) -> Result<u64, fs::Error> {
        let normalized = path.normalize();
        let state = self.shared.read();
        let device = state.get_device(&normalized).map(|d| d.device.clone());
        drop(state);

        if let Some(device) = device {
            device.ioctl(request, arg).map_err(fs::Error::from)
        } else {
            Err(fs::Error::FileNotFound)
        }
    }

    fn poll(&mut self, path: &Path) -> Result<PollState, fs::Error> {
        let normalized = path.normalize();
        let state = self.shared.read();
        let device = state.get_device(&normalized).map(|d| d.device.clone());
        drop(state);

        if let Some(device) = device {
            device.poll().map_err(fs::Error::from)
        } else {
            Err(fs::Error::FileNotFound)
        }
    }

    fn mmap(&mut self, path: &Path, offset: usize, length: usize) -> Result<MmapRegion, fs::Error> {
        let normalized = path.normalize();
        let state = self.shared.read();
        let device = state.get_device(&normalized).map(|d| d.device.clone());
        drop(state);

        if let Some(device) = device {
            device.mmap(offset, length).map_err(fs::Error::from)
        } else {
            Err(fs::Error::FileNotFound)
        }
    }
}

/// Register a new device node within devfs.
pub fn register_device(path: &Path, device: Arc<dyn DevFsDevice>) -> Result<(), DevFsError> {
    let normalized = path.normalize();
    let devfs = global_devfs();
    let mut devfs = devfs.write();
    devfs.insert_device(normalized, device)
}

/// Convenience helper that parses a string path before registering a device.
pub fn register_device_str(path: &str, device: Arc<dyn DevFsDevice>) -> Result<(), DevFsError> {
    let path = Path::parse(path).map_err(|_| DevFsError::InvalidPath)?;
    register_device(&path, device)
}

/// Remove an existing device node if present.
pub fn unregister_device(path: &Path) -> Result<(), DevFsError> {
    let normalized = path.normalize();
    let devfs = global_devfs();
    let mut devfs = devfs.write();
    devfs.remove_device(&normalized)
}

/// Convenience helper for unregistering devices via string paths.
pub fn unregister_device_str(path: &str) -> Result<(), DevFsError> {
    let path = Path::parse(path).map_err(|_| DevFsError::InvalidPath)?;
    unregister_device(&path)
}
