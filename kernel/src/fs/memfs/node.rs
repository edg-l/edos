use alloc::{string::String, vec::Vec};

use crate::fs::{File, FileKind, FileTime};

#[derive(Debug, Clone)]
pub struct Node {
    pub id: u32,
    // If its a directory
    pub childs: Vec<u32>,
    /// Backing store for a file or the target of a symlink.
    ///
    /// This is storage, not the file: `file.size` is the authoritative length
    /// and `content` may be longer, because a page writeback stores a whole
    /// 4 KiB page. Everything reading file data clamps to `file.size`.
    pub content: Vec<u8>,
    pub file: File,
}

impl Node {
    pub fn new(id: u32, name: String, kind: FileKind) -> Self {
        let now = FileTime::now();
        Node {
            id,
            childs: Vec::new(),
            content: Vec::new(),
            file: File {
                name,
                kind,
                size: 0,
                attrs: crate::fs::FileAttrs {
                    readonly: false,
                    hidden: false,
                    system: false,
                    archive: false,
                },
                created: Some(now),
                accessed: Some(now),
                modified: Some(now),
            },
        }
    }
}
