use crate::{
    FT_DIR, FT_FIFO, FT_REG_FILE, FT_SYMLINK, FT_UNKNOWN, INODE_FLAG_INLINE_DATA, S_IFDIR, S_IFIFO,
    S_IFLNK, S_IFMT, S_IFREG,
};

/// Size of the `data_area` inside an inode, in bytes.
pub const INODE_DATA_AREA_SIZE: usize = 176;

/// Inode structure. All inodes are 256 bytes, stored consecutively in the
/// inode table of their block group.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfsInode {
    /// File type (upper 4 bits) + Unix permission bits (lower 12 bits).
    pub mode: u16,
    /// Owner user ID.
    pub uid: u16,
    /// Owner group ID.
    pub gid: u16,
    /// Hard link count. Inode is freed when this reaches 0.
    pub link_count: u16,
    /// File size in bytes.
    pub size: u64,
    /// Number of filesystem blocks allocated to this inode.
    pub blocks: u64,
    /// Inode flags (see `INODE_FLAG_*` constants).
    pub flags: u32,
    /// Next inode in the orphan chain, or 0 for the end of it.
    ///
    /// Meaningful only while this inode is on the chain — that is, between
    /// losing its last name and having its storage freed. See the orphan list in
    /// `doc/efs.md` §14. Inode 0 is never allocated, which is what makes it the
    /// end-of-chain sentinel, and an image written before the chain existed has
    /// zero here and so reads as a chain of length zero.
    pub orphan_next: u32,
    /// Creation time: seconds since Unix epoch.
    pub ctime_sec: u64,
    /// Creation time: nanoseconds component (0..=999_999_999).
    pub ctime_nsec: u32,
    /// Reserved; must be zero.
    pub reserved2: u32,
    /// Modification time: seconds since Unix epoch.
    pub mtime_sec: u64,
    /// Modification time: nanoseconds component.
    pub mtime_nsec: u32,
    /// Reserved; must be zero.
    pub reserved3: u32,
    /// Access time: seconds since Unix epoch.
    pub atime_sec: u64,
    /// Access time: nanoseconds component.
    pub atime_nsec: u32,
    /// CRC32 of the full 256-byte inode with this field zeroed during computation.
    pub checksum: u32,
    /// Extent tree root or inline data (see `INODE_FLAG_INLINE_DATA`).
    pub data_area: [u8; INODE_DATA_AREA_SIZE],
}

const _: () = assert!(core::mem::size_of::<EfsInode>() == 256);

impl EfsInode {
    /// A fresh inode of `mode`, timestamped `(secs, nanos)` on all three
    /// stamps, owned by root, with one link, no size and no blocks.
    ///
    /// The caller sets whatever it differs in and stamps the checksum last —
    /// `checksum_inode` covers every other field, so computing it here would
    /// only be right for an inode nobody touched afterwards. This exists
    /// because the same twenty-field literal was written out in the kernel and
    /// four times in `efs-mkfs`: a field added to the struct has to reach every
    /// one of them, and the compiler only says so for the ones it can see.
    pub fn new(mode: u16, ts: (u64, u32)) -> Self {
        EfsInode {
            mode,
            uid: 0,
            gid: 0,
            link_count: 1,
            size: 0,
            blocks: 0,
            flags: 0,
            orphan_next: 0,
            ctime_sec: ts.0,
            ctime_nsec: ts.1,
            reserved2: 0,
            mtime_sec: ts.0,
            mtime_nsec: ts.1,
            reserved3: 0,
            atime_sec: ts.0,
            atime_nsec: ts.1,
            checksum: 0,
            data_area: [0u8; INODE_DATA_AREA_SIZE],
        }
    }

    /// Returns `true` if this inode represents a directory.
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    /// Returns `true` if this inode represents a regular file.
    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    /// Returns `true` if this inode represents a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }

    /// Returns `true` if this inode represents a named pipe (FIFO).
    pub fn is_fifo(&self) -> bool {
        self.mode & S_IFMT == S_IFIFO
    }

    /// Returns `true` if file data is stored inline in `data_area`.
    pub fn is_inline_data(&self) -> bool {
        self.flags & INODE_FLAG_INLINE_DATA != 0
    }

    /// Returns the inline data slice.
    ///
    /// Only valid when `is_inline_data()` returns `true`. The returned slice
    /// has length `self.size` bytes.
    pub fn inline_data(&self) -> &[u8] {
        let len = (self.size as usize).min(self.data_area.len());
        &self.data_area[..len]
    }

    /// Returns the `file_type` byte suitable for use in a directory entry.
    pub fn file_type_for_dir_entry(&self) -> u8 {
        if self.is_file() {
            FT_REG_FILE
        } else if self.is_dir() {
            FT_DIR
        } else if self.is_symlink() {
            FT_SYMLINK
        } else if self.is_fifo() {
            FT_FIFO
        } else {
            FT_UNKNOWN
        }
    }
}
