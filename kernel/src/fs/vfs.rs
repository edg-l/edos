use alloc::{boxed::Box, collections::btree_map::BTreeMap, vec::Vec};

use crate::fs::{FileSystem, path::Path};

/// Holds all fs, also adds a caching layer
pub struct VirtualFs {
    pub partitions: Vec<Box<dyn FileSystem>>,
    /// Mount point -> index into partitions.
    pub mount_points: BTreeMap<Path, usize>,
}
