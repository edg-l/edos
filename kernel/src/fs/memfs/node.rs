use alloc::{string::String, vec::Vec};

use crate::fs::{File, FileKind, FileTime};

#[derive(Debug, Clone)]
pub struct Node {
    pub id: u32,
    // If its a directory
    pub childs: Vec<u32>,
    // If its a file,
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
