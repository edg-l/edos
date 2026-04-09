#![no_std]

pub mod block_group;
pub mod crc32;
pub mod dir_entry;
pub mod extent;
pub mod inode;
pub mod superblock;

// Re-export all public types for convenience.
pub use block_group::EfsBlockGroupDesc;
pub use crc32::{checksum_block_group_desc, checksum_inode, checksum_superblock, crc32};
pub use dir_entry::{DIR_ENTRY_HEADER_SIZE, EfsDirEntryHeader, dir_entry_min_size};
pub use extent::{EfsExtent, EfsExtentHeader, EfsExtentIndex, MAX_INLINE_EXTENTS};
pub use inode::{EfsInode, INODE_DATA_AREA_SIZE};
pub use superblock::{EfsSuperblock, has_superblock_backup};

// ---- Magic and version ----

/// On-disk magic number (ASCII "EFS!").
pub const EFS_MAGIC: u32 = 0x4546_5321;

/// On-disk format version implemented by this crate.
pub const EFS_VERSION: u32 = 1;

/// Magic value that validates an extent tree node header.
pub const EXTENT_MAGIC: u16 = 0xEF10;

// ---- Reserved inodes ----

/// Inode number 0 is invalid; never allocated.
pub const EFS_INO_INVALID: u64 = 0;

/// Inode number of the root directory.
pub const EFS_ROOT_INO: u64 = 1;

// ---- Feature flag bits ----

/// Compatible feature: filesystem was created with TRIM/discard support.
pub const COMPAT_DISCARD: u64 = 1 << 0;

// ---- Inode flag bits ----

/// Inode flag: file data is stored inline in `data_area`; no extent tree.
pub const INODE_FLAG_INLINE_DATA: u32 = 1 << 0;

/// Inode flag: file content and metadata cannot be modified.
pub const INODE_FLAG_IMMUTABLE: u32 = 1 << 1;

// ---- File type constants for `mode` field ----

/// Mask for the file type bits in `mode` (upper 4 bits).
pub const S_IFMT: u16 = 0xF000;

/// `mode` file type: regular file.
pub const S_IFREG: u16 = 0x8000;

/// `mode` file type: directory.
pub const S_IFDIR: u16 = 0x4000;

/// `mode` file type: symbolic link.
pub const S_IFLNK: u16 = 0xA000;

// ---- Permission bits ----

/// Owner read/write/execute permission bits.
pub const S_IRWXU: u16 = 0o700;

/// Group read/write/execute permission bits.
pub const S_IRWXG: u16 = 0o070;

/// Others read/write/execute permission bits.
pub const S_IRWXO: u16 = 0o007;

// ---- Directory entry file type values ----

/// Directory entry file type: unknown.
pub const FT_UNKNOWN: u8 = 0;

/// Directory entry file type: regular file.
pub const FT_REG_FILE: u8 = 1;

/// Directory entry file type: directory.
pub const FT_DIR: u8 = 2;

/// Directory entry file type: symbolic link.
pub const FT_SYMLINK: u8 = 7;

// ---- Filename limits ----

/// Maximum filename length in bytes.
pub const EFS_MAX_NAME_LEN: usize = 255;

// ---- Block size defaults ----

/// Default block size expressed as log2. 12 = 4096 bytes.
pub const DEFAULT_BLOCK_SIZE_LOG2: u32 = 12;
