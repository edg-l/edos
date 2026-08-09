//! Memory based filesystem
//!
//! Root has node id 0.
//!
//! # PageCacheOps coherency note (v1)
//!
//! MAP_SHARED pages are NOT coherent with concurrent path-based
//! `write_bytes` / `write_bytes_ino` on the same file. `flush_page`
//! overwrites `node.content` wholesale from the cached frame, so a
//! concurrent path-based write racing against a dirty-page writeback will
//! have its data silently overwritten. This mirrors the v1 design and will
//! be revisited if memfs gains a use case beyond /tmp.

use alloc::{
    collections::btree_map::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};

use crate::{
    fs::{Error, FileSystem, memfs::node::Node, page_cache::PageCacheOps, path::Path},
    thread::rwlock::RwLock as BlockingRwLock,
};

use super::{FileKind, FileTime, MAX_SYMLINK_HOPS, splice_symlink};

mod node;

struct MemfsInner {
    nodes: BTreeMap<u32, Node>,
    next_id: u32,
}

impl MemfsInner {
    fn get_node(&self, id: u32) -> Result<&Node, Error> {
        self.nodes.get(&id).ok_or(Error::Corrupted)
    }

    fn get_node_mut(&mut self, id: u32) -> Result<&mut Node, Error> {
        self.nodes.get_mut(&id).ok_or(Error::Corrupted)
    }

    /// Resolve a path, following symbolic links including one named by the
    /// last component.
    fn find_node(&self, path: &Path) -> Result<Option<u32>, Error> {
        self.walk(path, true)
    }

    /// Resolve a path whose last component is left unfollowed, so a symbolic
    /// link resolves to the link itself.
    fn find_node_nofollow(&self, path: &Path) -> Result<Option<u32>, Error> {
        self.walk(path, false)
    }

    fn walk(&self, path: &Path, follow_final: bool) -> Result<Option<u32>, Error> {
        let mut components: Vec<String> = path.components().to_vec();
        let mut hops = 0u32;

        'walk: loop {
            let mut current = self.nodes.get(&0).unwrap();
            let mut resolved: Vec<String> = Vec::new();

            let mut index = 0;
            while index < components.len() {
                let mut found = None;
                for child in &current.childs {
                    let child_node = self.get_node(*child)?;
                    if child_node.file.name == components[index] {
                        found = Some(child_node);
                        break;
                    }
                }
                let Some(node) = found else {
                    return Ok(None);
                };

                let is_last = index + 1 == components.len();
                if node.file.kind == FileKind::Symlink && (!is_last || follow_final) {
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        return Err(Error::TooManyLinks);
                    }
                    let target =
                        core::str::from_utf8(&node.content).map_err(|_| Error::Corrupted)?;
                    components = splice_symlink(&resolved, target, &components[index + 1..]);
                    continue 'walk;
                }

                current = node;
                resolved.push(components[index].clone());
                index += 1;
            }

            return Ok(Some(current.id));
        }
    }

    fn get_all_child_ids(&self, id: u32) -> Result<Vec<u32>, Error> {
        let mut ids = Vec::new();
        let node = self.get_node(id)?;

        for child in &node.childs {
            ids.push(*child);
            ids.extend(self.get_all_child_ids(*child)?);
        }

        Ok(ids)
    }
}

/// Memory based filesystem, volatile.
pub struct Memfs {
    inner: BlockingRwLock<MemfsInner>,
}

impl Memfs {
    pub fn new() -> Result<Self, Error> {
        let root = Node::new(0, String::new(), FileKind::Directory);
        let mut nodes = BTreeMap::new();
        nodes.insert(0, root);
        Ok(Self {
            inner: BlockingRwLock::new(MemfsInner { nodes, next_id: 1 }),
        })
    }
}

impl FileSystem for Memfs {
    fn list_files(&self, path: &Path) -> Result<Vec<super::File>, Error> {
        let path = path.normalize();
        let inner = self.inner.read();
        if let Some(node_id) = inner.find_node(&path)? {
            let node = inner.get_node(node_id)?;

            if node.file.kind != FileKind::Directory {
                return Err(Error::NotADir);
            }

            let mut files = Vec::new();

            for child in &node.childs {
                let child = inner.get_node(*child)?;
                files.push(child.file.clone());
            }

            Ok(files)
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn read_bytes(&self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
        let path = path.normalize();
        let inner = self.inner.read();
        if let Some(node_id) = inner.find_node(&path)? {
            let node = inner.get_node(node_id)?;

            if node.file.kind != FileKind::File {
                return Err(Error::NotAFile);
            }

            if offset >= node.content.len() {
                return Ok(Vec::new());
            }

            let upper_bound = node.content.len().min(offset + count);

            let data = node.content.get(offset..upper_bound).unwrap().to_vec();

            Ok(data)
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn write_bytes(&self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
        let path = path.normalize();
        let mut inner = self.inner.write();
        if let Some(node_id) = inner.find_node(&path)? {
            let node = inner.get_node_mut(node_id)?;

            if node.file.kind != FileKind::File {
                return Err(Error::NotAFile);
            }

            if offset > node.content.len() {
                return Err(Error::IoError);
            }

            if offset == node.content.len() {
                node.content.extend(data);
                return Ok(data.len() as u64);
            } else {
                let tail = &mut node.content[offset..];
                let overwrite_len = tail.len().min(data.len());
                tail[..overwrite_len].copy_from_slice(&data[..overwrite_len]);
                node.content.extend_from_slice(&data[overwrite_len..]);
            }

            Ok(data.len() as u64)
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn symlink(&self, target: &str, path: &Path) -> Result<(), Error> {
        if target.is_empty() {
            return Err(Error::InvalidArgument);
        }
        let path = path.normalize();
        let Some(parent) = path.parent() else {
            return Err(Error::InvalidArgument);
        };

        let mut inner = self.inner.write();
        let Some(parent_node_id) = inner.find_node(&parent)? else {
            return Err(Error::FileNotFound);
        };
        if inner.get_node(parent_node_id)?.file.kind != FileKind::Directory {
            return Err(Error::NotADir);
        }
        if inner.find_node_nofollow(&path)?.is_some() {
            return Err(Error::InvalidArgument);
        }

        let current_id = inner.next_id;
        let next_id = current_id.checked_add(1).ok_or(Error::IoError)?;
        let mut node = Node::new(current_id, path.filename(), FileKind::Symlink);
        node.content = target.as_bytes().to_vec();
        node.file.size = target.len() as u64;

        inner.get_node_mut(parent_node_id)?.childs.push(node.id);
        inner.next_id = next_id;
        inner.nodes.insert(node.id, node);
        Ok(())
    }

    fn read_link(&self, path: &Path) -> Result<String, Error> {
        let path = path.normalize();
        let inner = self.inner.read();
        let Some(node_id) = inner.find_node_nofollow(&path)? else {
            return Err(Error::FileNotFound);
        };
        let node = inner.get_node(node_id)?;
        if node.file.kind != FileKind::Symlink {
            return Err(Error::InvalidArgument);
        }
        core::str::from_utf8(&node.content)
            .map(ToString::to_string)
            .map_err(|_| Error::Corrupted)
    }

    fn create_file(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let Some(parent) = path.parent() else {
            return Err(Error::IoError);
        };

        let mut inner = self.inner.write();
        if let Some(parent_node_id) = inner.find_node(&parent)? {
            let current_id = inner.next_id;
            let next_id = current_id.checked_add(1).ok_or(Error::IoError)?;
            let parent_node = inner.get_node_mut(parent_node_id)?;

            if parent_node.file.kind != FileKind::Directory {
                return Err(Error::IoError);
            }

            let name = path.filename();

            let node = Node::new(current_id, name, FileKind::File);
            parent_node.childs.push(node.id);
            inner.next_id = next_id;
            inner.nodes.insert(node.id, node);

            Ok(())
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn create_dir(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let Some(parent) = path.parent() else {
            return Err(Error::IoError);
        };

        let mut inner = self.inner.write();
        if let Some(parent_node_id) = inner.find_node(&parent)? {
            let current_id = inner.next_id;
            let next_id = current_id.checked_add(1).ok_or(Error::IoError)?;
            let parent_node = inner.get_node_mut(parent_node_id)?;

            if parent_node.file.kind != FileKind::Directory {
                return Err(Error::IoError);
            }

            let name = path.filename();

            let node = Node::new(current_id, name, FileKind::Directory);
            parent_node.childs.push(node.id);
            inner.next_id = next_id;
            inner.nodes.insert(node.id, node);

            Ok(())
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn remove_dir(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();

        if path.is_root() {
            return Err(Error::IoError);
        }

        let Some(parent) = path.parent() else {
            return Err(Error::IoError);
        };

        let mut inner = self.inner.write();
        if let Some(parent_node_id) = inner.find_node(&parent)? {
            let parent_node = inner.get_node(parent_node_id)?;

            if parent_node.file.kind != FileKind::Directory {
                return Err(Error::IoError);
            }

            let mut idx = None;

            let name = path.filename();

            for (i, id) in parent_node.childs.iter().enumerate() {
                let child = inner.get_node(*id)?;

                if child.file.name == name {
                    idx = Some((i, *id));

                    if child.file.kind != FileKind::Directory {
                        return Err(Error::NotADir);
                    }
                    break;
                }
            }

            if let Some((i, id)) = idx {
                let child_ids = inner.get_all_child_ids(id)?;

                {
                    let parent_node = inner.get_node_mut(parent_node_id)?;
                    parent_node.childs.remove(i);
                }

                inner.nodes.remove(&id);

                for id in child_ids {
                    inner.nodes.remove(&id);
                }

                Ok(())
            } else {
                Err(Error::FileNotFound)
            }
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn remove_file(&self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();

        if path.is_root() {
            return Err(Error::IoError);
        }

        let Some(parent) = path.parent() else {
            return Err(Error::IoError);
        };

        let mut inner = self.inner.write();
        if let Some(parent_node_id) = inner.find_node(&parent)? {
            let parent_node = inner.get_node(parent_node_id)?;

            if parent_node.file.kind != FileKind::Directory {
                return Err(Error::IoError);
            }

            let mut idx = None;

            let name = path.filename();

            for (i, id) in parent_node.childs.iter().enumerate() {
                let child = inner.get_node(*id)?;

                if child.file.name == name {
                    idx = Some(i);

                    // Unlinking a symbolic link removes the link, never its target.
                    if !matches!(child.file.kind, FileKind::File | FileKind::Symlink) {
                        return Err(Error::NotAFile);
                    }
                    break;
                }
            }

            if let Some(i) = idx {
                // Detach from parent's childs list so path lookups fail. The
                // node itself stays in `nodes` keyed by ino so already-open
                // fds and live mmap mappings can still fill/flush through
                // ino-based lookups. `evict_inode` drops the node when the
                // final `Arc<VfsInode>` is released (Linux i_count model).
                let parent_node = inner.get_node_mut(parent_node_id)?;
                parent_node.childs.remove(i);
                Ok(())
            } else {
                Err(Error::FileNotFound)
            }
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn file_info(&self, path: &Path) -> Result<super::File, Error> {
        let path = path.normalize();
        let inner = self.inner.read();
        if let Some(node_id) = inner.find_node(&path)? {
            let node = inner.get_node(node_id)?;

            Ok(node.file.clone())
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn flush(&self) -> Result<(), Error> {
        Ok(())
    }

    fn statfs(&self) -> Result<super::StatFs, Error> {
        let inner = self.inner.read();
        let total = inner.nodes.len() as u64;
        let mut volume_name = [0u8; 64];
        volume_name[..4].copy_from_slice(b"tmp\0");
        Ok(super::StatFs {
            fs_type: "memfs",
            block_size: 4096,
            total_blocks: 0,
            free_blocks: 0,
            total_inodes: total,
            free_inodes: 0,
            volume_name,
            version: 0,
            block_groups: 0,
        })
    }

    fn resolve_inode(&self, path: &Path) -> Result<u64, Error> {
        let path = path.normalize();
        let inner = self.inner.read();
        inner
            .find_node(&path)?
            .map(|id| id as u64)
            .ok_or(Error::FileNotFound)
    }

    fn truncate(&self, path: &Path, size: u64) -> Result<(), Error> {
        let path = path.normalize();
        let mut inner = self.inner.write();
        if let Some(node_id) = inner.find_node(&path)? {
            let node = inner.get_node_mut(node_id)?;
            if node.file.kind != FileKind::File {
                return Err(Error::NotAFile);
            }
            let new_size = size as usize;
            if new_size <= node.content.len() {
                node.content.truncate(new_size);
            } else {
                node.content.resize(new_size, 0u8);
            }
            node.file.size = node.content.len() as u64;
            Ok(())
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn set_times(&self, path: &Path, atime: Option<u64>, mtime: Option<u64>) -> Result<(), Error> {
        let path = path.normalize();
        let mut inner = self.inner.write();
        let node_id = inner.find_node(&path)?.ok_or(Error::FileNotFound)?;
        let node = inner.get_node_mut(node_id)?;
        if let Some(secs) = atime {
            node.file.accessed = Some(FileTime::from_unix_secs(secs));
        }
        if let Some(secs) = mtime {
            node.file.modified = Some(FileTime::from_unix_secs(secs));
        }
        Ok(())
    }

    fn read_bytes_ino(&self, ino: u64, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
        let id = u32::try_from(ino).map_err(|_| Error::Corrupted)?;
        let inner = self.inner.read();
        let node = inner.get_node(id)?;
        if node.file.kind != FileKind::File {
            return Err(Error::NotAFile);
        }
        if offset >= node.content.len() {
            return Ok(Vec::new());
        }
        let upper_bound = node.content.len().min(offset + count);
        Ok(node.content[offset..upper_bound].to_vec())
    }

    fn write_bytes_ino(&self, ino: u64, offset: usize, data: &[u8]) -> Result<u64, Error> {
        let id = u32::try_from(ino).map_err(|_| Error::Corrupted)?;
        let mut inner = self.inner.write();
        let node = inner.get_node_mut(id)?;
        if node.file.kind != FileKind::File {
            return Err(Error::NotAFile);
        }
        if offset > node.content.len() {
            return Err(Error::IoError);
        }
        if offset == node.content.len() {
            node.content.extend(data);
        } else {
            let tail = &mut node.content[offset..];
            let overwrite_len = tail.len().min(data.len());
            tail[..overwrite_len].copy_from_slice(&data[..overwrite_len]);
            node.content.extend_from_slice(&data[overwrite_len..]);
        }
        Ok(data.len() as u64)
    }

    fn file_size_ino(&self, ino: u64) -> Result<u64, Error> {
        let id = u32::try_from(ino).map_err(|_| Error::Corrupted)?;
        let inner = self.inner.read();
        let node = inner.get_node(id)?;
        Ok(node.content.len() as u64)
    }

    fn as_page_cache_ops(&self) -> Option<&dyn PageCacheOps> {
        Some(self)
    }

    fn evict_inode(&self, ino: u64) -> Result<(), Error> {
        // Called from VfsInode::drop when an orphaned memfs node's final
        // Arc ref is released. Drop the node so its `content` Vec frees.
        let id = u32::try_from(ino).map_err(|_| Error::Corrupted)?;
        self.inner.write().nodes.remove(&id);
        Ok(())
    }

    fn rename(&self, old_path: &Path, new_path: &Path) -> Result<(), Error> {
        let old_path = old_path.normalize();
        let new_path = new_path.normalize();

        if old_path.is_root() || new_path.is_root() {
            return Err(Error::IoError);
        }

        let old_parent = old_path.parent().ok_or(Error::IoError)?;
        let new_parent = new_path.parent().ok_or(Error::IoError)?;

        let mut inner = self.inner.write();

        let old_parent_id = inner.find_node(&old_parent)?.ok_or(Error::FileNotFound)?;
        let new_parent_id = inner.find_node(&new_parent)?.ok_or(Error::FileNotFound)?;

        let old_name = old_path.filename();
        let new_name = new_path.filename();

        // Find the node id to move
        let node_id = {
            let parent = inner.get_node(old_parent_id)?;
            let mut found = None;
            for &child_id in &parent.childs {
                let child = inner.get_node(child_id)?;
                if child.file.name == old_name {
                    found = Some(child_id);
                    break;
                }
            }
            found.ok_or(Error::FileNotFound)?
        };

        // Remove from old parent
        {
            let parent = inner.get_node_mut(old_parent_id)?;
            parent.childs.retain(|&id| id != node_id);
        }

        // Update the node's name
        {
            let node = inner.get_node_mut(node_id)?;
            node.file.name = new_name;
        }

        // Add to new parent
        {
            let parent = inner.get_node_mut(new_parent_id)?;
            parent.childs.push(node_id);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PageCacheOps implementation
// ---------------------------------------------------------------------------

impl PageCacheOps for Memfs {
    /// Copy a 4 KiB page of file content into `buf`.
    ///
    /// `node.content` is the source of truth. No disk I/O.
    fn fill_page(&self, ino: u64, page_index: u64, buf: &mut [u8]) -> Result<usize, Error> {
        debug_assert_eq!(buf.len(), 4096);
        let id = u32::try_from(ino).map_err(|_| Error::Corrupted)?;
        let inner = self.inner.read();
        let node = inner.get_node(id)?;
        if node.file.kind != FileKind::File {
            return Err(Error::NotAFile);
        }
        let offset = (page_index as usize) * 4096;
        if offset >= node.content.len() {
            buf.fill(0);
            return Ok(0);
        }
        let valid = core::cmp::min(node.content.len() - offset, 4096);
        buf[..valid].copy_from_slice(&node.content[offset..offset + valid]);
        buf[valid..].fill(0);
        Ok(valid)
    }

    /// Write a page from the cache back into `node.content`.
    ///
    /// VFS callers always pass `valid_bytes = 4096` regardless of the true tail;
    /// trusting that would inflate the file because `node.content` IS the file
    /// storage (unlike disk-backed FSes where block size and file size are
    /// independent). Instead we copy the whole page into content but leave
    /// `node.file.size` untouched; `update_size` is authoritative for size.
    ///
    /// Note: see module-level doc for the MAP_SHARED / path-based write
    /// coherency caveat.
    fn flush_page(
        &self,
        ino: u64,
        page_index: u64,
        buf: &[u8],
        _valid_bytes: usize,
    ) -> Result<(), Error> {
        debug_assert_eq!(buf.len(), 4096);
        let id = u32::try_from(ino).map_err(|_| Error::Corrupted)?;
        let mut inner = self.inner.write();
        let node = inner.get_node_mut(id)?;
        if node.file.kind != FileKind::File {
            return Err(Error::NotAFile);
        }
        let offset = (page_index as usize) * 4096;
        let end = offset + 4096;
        if end > node.content.len() {
            node.content.resize(end, 0u8);
        }
        node.content[offset..end].copy_from_slice(buf);
        // Do NOT touch node.file.size here; update_size sets the authoritative
        // size. Writing 4096 bytes into content past the real EOF is harmless
        // because reads are clamped to file.size at the VFS layer.
        Ok(())
    }

    /// Set the on-disk file size. Grow-only per the `PageCacheOps` contract.
    ///
    /// `node.content` may be larger than `new_size` when called after a
    /// `flush_page` that wrote a full 4 KiB page for a small file; that is
    /// fine because reads clamp to `file.size`.
    fn update_size(&self, ino: u64, new_size: u64) -> Result<(), Error> {
        let id = u32::try_from(ino).map_err(|_| Error::Corrupted)?;
        let mut inner = self.inner.write();
        let node = inner.get_node_mut(id)?;
        if node.file.kind != FileKind::File {
            return Err(Error::NotAFile);
        }
        if new_size <= node.file.size {
            return Ok(());
        }
        let new_size_usize = new_size as usize;
        if new_size_usize > node.content.len() {
            node.content.resize(new_size_usize, 0u8);
        }
        node.file.size = new_size;
        Ok(())
    }
}
