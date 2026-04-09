//! Memory based filesystem
//!
//! Root has node id 0.

use alloc::{
    collections::btree_map::BTreeMap,
    string::String,
    vec::Vec,
};

use crate::fs::{Error, FileSystem, memfs::node::Node, path::Path};

use super::FileKind;

mod node;

/// Memory based filesystem, volatile.
#[derive(Debug)]
pub struct Memfs {
    nodes: BTreeMap<u32, Node>,
    next_id: u32,
}

impl Memfs {
    pub fn new() -> Result<Self, Error> {
        let root = Node::new(0, String::new(), FileKind::Directory);
        let mut nodes = BTreeMap::new();
        nodes.insert(0, root);
        Ok(Self { nodes, next_id: 1 })
    }

    pub fn find_node(&self, path: &Path) -> Result<Option<u32>, Error> {
        let mut current = self.nodes.get(&0).unwrap();

        for component in path.components() {
            let mut found = false;
            for child in &current.childs {
                let child_node = self.get_node(*child)?;

                if &child_node.file.name == component {
                    current = child_node;
                    found = true;
                    break;
                }
            }

            if !found {
                return Ok(None);
            }
        }

        Ok(Some(current.id))
    }

    /// Returns a corrupted error if not found.
    fn get_node(&self, id: u32) -> Result<&Node, Error> {
        self.nodes.get(&id).ok_or(Error::Corrupted)
    }

    /// Returns a corrupted error if not found.
    fn get_node_mut(&mut self, id: u32) -> Result<&mut Node, Error> {
        self.nodes.get_mut(&id).ok_or(Error::Corrupted)
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

impl FileSystem for Memfs {
    fn list_files(&mut self, path: &Path) -> Result<Vec<super::File>, Error> {
        let path = path.normalize();
        if let Some(node) = self.find_node(&path)? {
            let node = self.get_node(node)?;

            if node.file.kind != FileKind::Directory {
                return Err(Error::NotADir);
            }

            let mut files = Vec::new();

            for child in &node.childs {
                let child = self.get_node(*child)?;
                files.push(child.file.clone());
            }

            Ok(files)
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn read_bytes(&mut self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
        let path = path.normalize();
        if let Some(node) = self.find_node(&path)? {
            let node = self.get_node(node)?;

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

    fn write_bytes(&mut self, path: &Path, offset: usize, data: &[u8]) -> Result<u64, Error> {
        let path = path.normalize();
        if let Some(node) = self.find_node(&path)? {
            let node = self.get_node_mut(node)?;

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

    fn create_file(&mut self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let Some(parent) = path.parent() else {
            return Err(Error::IoError);
        };

        if let Some(parent_node) = self.find_node(&parent)? {
            let current_id = self.next_id;
            let next_id = current_id.checked_add(1).ok_or(Error::IoError)?;
            let parent_node = self.get_node_mut(parent_node)?;

            if parent_node.file.kind != FileKind::Directory {
                return Err(Error::IoError);
            }

            let name = path.filename();

            let node = Node::new(current_id, name, FileKind::File);
            parent_node.childs.push(node.id);
            self.next_id = next_id;
            self.nodes.insert(node.id, node);

            Ok(())
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn create_dir(&mut self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();
        let Some(parent) = path.parent() else {
            return Err(Error::IoError);
        };

        if let Some(parent_node) = self.find_node(&parent)? {
            let current_id = self.next_id;
            let next_id = current_id.checked_add(1).ok_or(Error::IoError)?;
            let parent_node = self.get_node_mut(parent_node)?;

            if parent_node.file.kind != FileKind::Directory {
                return Err(Error::IoError);
            }

            let name = path.filename();

            let node = Node::new(current_id, name, FileKind::Directory);
            parent_node.childs.push(node.id);
            self.next_id = next_id;
            self.nodes.insert(node.id, node);

            Ok(())
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn remove_dir(&mut self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();

        if path.is_root() {
            return Err(Error::IoError);
        }

        let Some(parent) = path.parent() else {
            return Err(Error::IoError);
        };

        if let Some(parent_node_id) = self.find_node(&parent)? {
            let parent_node = self.get_node(parent_node_id)?;

            if parent_node.file.kind != FileKind::Directory {
                return Err(Error::IoError);
            }

            let mut idx = None;

            let name = path.filename();

            for (i, id) in parent_node.childs.iter().enumerate() {
                let child = self.get_node(*id)?;

                if child.file.name == name {
                    idx = Some((i, *id));

                    if child.file.kind != FileKind::Directory {
                        return Err(Error::NotADir);
                    }
                    break;
                }
            }

            if let Some((i, id)) = idx {
                let child_ids = self.get_all_child_ids(id)?;

                {
                    let parent_node = self.get_node_mut(parent_node_id)?;
                    parent_node.childs.remove(i);
                }

                self.nodes.remove(&id);

                for id in child_ids {
                    self.nodes.remove(&id);
                }

                Ok(())
            } else {
                Err(Error::FileNotFound)
            }
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn remove_file(&mut self, path: &Path) -> Result<(), Error> {
        let path = path.normalize();

        if path.is_root() {
            return Err(Error::IoError);
        }

        let Some(parent) = path.parent() else {
            return Err(Error::IoError);
        };

        if let Some(parent_node_id) = self.find_node(&parent)? {
            let parent_node = self.get_node(parent_node_id)?;

            if parent_node.file.kind != FileKind::Directory {
                return Err(Error::IoError);
            }

            let mut idx = None;

            let name = path.filename();

            for (i, id) in parent_node.childs.iter().enumerate() {
                let child = self.get_node(*id)?;

                if child.file.name == name {
                    idx = Some((i, *id));

                    if child.file.kind != FileKind::File {
                        return Err(Error::NotAFile);
                    }
                    break;
                }
            }

            if let Some((i, id)) = idx {
                {
                    let parent_node = self.get_node_mut(parent_node_id)?;
                    parent_node.childs.remove(i);
                }

                self.nodes.remove(&id);

                Ok(())
            } else {
                Err(Error::FileNotFound)
            }
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn file_info(&mut self, path: &Path) -> Result<super::File, Error> {
        let path = path.normalize();
        if let Some(node) = self.find_node(&path)? {
            let node = self.get_node(node)?;

            Ok(node.file.clone())
        } else {
            Err(Error::FileNotFound)
        }
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn truncate(&mut self, path: &Path, size: u64) -> Result<(), Error> {
        let path = path.normalize();
        if let Some(node_id) = self.find_node(&path)? {
            let node = self.get_node_mut(node_id)?;
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

    fn rename(&mut self, old_path: &Path, new_path: &Path) -> Result<(), Error> {
        let old_path = old_path.normalize();
        let new_path = new_path.normalize();

        if old_path.is_root() || new_path.is_root() {
            return Err(Error::IoError);
        }

        let old_parent = old_path.parent().ok_or(Error::IoError)?;
        let new_parent = new_path.parent().ok_or(Error::IoError)?;

        let old_parent_id = self.find_node(&old_parent)?.ok_or(Error::FileNotFound)?;
        let new_parent_id = self.find_node(&new_parent)?.ok_or(Error::FileNotFound)?;

        let old_name = old_path.filename();
        let new_name = new_path.filename();

        // Find the node id to move
        let node_id = {
            let parent = self.get_node(old_parent_id)?;
            let mut found = None;
            for &child_id in &parent.childs {
                let child = self.get_node(child_id)?;
                if child.file.name == old_name {
                    found = Some(child_id);
                    break;
                }
            }
            found.ok_or(Error::FileNotFound)?
        };

        // Remove from old parent
        {
            let parent = self.get_node_mut(old_parent_id)?;
            parent.childs.retain(|&id| id != node_id);
        }

        // Update the node's name
        {
            let node = self.get_node_mut(node_id)?;
            node.file.name = new_name;
        }

        // Add to new parent
        {
            let parent = self.get_node_mut(new_parent_id)?;
            parent.childs.push(node_id);
        }

        Ok(())
    }
}
