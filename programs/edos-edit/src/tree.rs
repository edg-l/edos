//! The sidebar's model: one row per visible node, read lazily as directories
//! expand.
//!
//! [`Tree::new`] reads only the root's own children; nothing past that is
//! read until [`Tree::toggle`] expands a directory, which is what keeps
//! opening a tree over a large directory instant regardless of how deep it
//! goes.
//!
//! Nothing re-reads a directory on its own. [`Tree::refresh`] is the only way
//! a file created after a folder was expanded becomes visible.

use std::fs;

/// One row of the tree: a name at some depth, and whether a directory is
/// showing its own children.
pub struct Node {
    pub name: String,
    pub path: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
}

/// The sidebar's whole visible state: every row currently on screen, in
/// order, and how far the sidebar is scrolled through them.
pub struct Tree {
    pub root: String,
    pub rows: Vec<Node>,
    pub scroll: usize,
}

impl Tree {
    /// Read `root`'s own children, initially collapsed.
    pub fn new(root: &str) -> Self {
        Self {
            root: root.to_string(),
            rows: list_dir(root, 0),
            scroll: 0,
        }
    }

    /// Expand or collapse the directory at `index`. Expanding reads its
    /// children for the first time and splices them in right after it;
    /// collapsing drops every row nested under it, so a folder revisited
    /// later reads its contents again rather than trusting a stale copy.
    pub fn toggle(&mut self, index: usize) {
        let Some(node) = self.rows.get(index) else {
            return;
        };
        if !node.is_dir {
            return;
        }
        let expanded = node.expanded;
        let depth = node.depth;
        let path = node.path.clone();

        if expanded {
            let end = self.rows[index + 1..]
                .iter()
                .position(|n| n.depth <= depth)
                .map_or(self.rows.len(), |offset| index + 1 + offset);
            self.rows.drain(index + 1..end);
        } else {
            let children = list_dir(&path, depth + 1);
            self.rows.splice(index + 1..index + 1, children);
        }
        self.rows[index].expanded = !expanded;
    }

    /// Re-read every directory currently on screen, keeping the same folders
    /// open. Nothing tells a program that a file appeared — the kernel has no
    /// change notification — so the sidebar is only ever as fresh as the last
    /// time something asked it to look, and a file written after the tree was
    /// built is invisible until then.
    pub fn refresh(&mut self) {
        let open: Vec<String> = self
            .rows
            .iter()
            .filter(|node| node.is_dir && node.expanded)
            .map(|node| node.path.clone())
            .collect();

        self.rows = list_dir(&self.root, 0);

        // `toggle` splices a directory's children in directly after it, so
        // walking forward re-expands nested folders as it reaches them: a
        // child's own row is visited on a later step of this same pass.
        let mut index = 0;
        while index < self.rows.len() {
            if self.rows[index].is_dir && open.contains(&self.rows[index].path) {
                self.toggle(index);
            }
            index += 1;
        }
    }
}

/// One directory's children, sorted directories-first then by name. Case is
/// ignored so a capitalized name does not sort ahead of every lowercase one.
/// An unreadable directory (permissions, or removed since the row was drawn)
/// simply has no children rather than failing the whole tree.
fn list_dir(path: &str, depth: usize) -> Vec<Node> {
    let Ok(reader) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut nodes: Vec<Node> = reader
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            Node {
                path: join(path, &name),
                name,
                depth,
                is_dir,
                expanded: false,
            }
        })
        .collect();
    nodes.sort_by(|a, b| {
        let group = |n: &Node| if n.is_dir { 0 } else { 1 };
        group(a)
            .cmp(&group(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    nodes
}

/// Join a directory and a name into a path, without doubling the separator
/// at the root.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}
