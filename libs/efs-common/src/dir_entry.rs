/// Size of the fixed-length portion of a directory entry header, in bytes.
pub const DIR_ENTRY_HEADER_SIZE: usize = 12;

/// Fixed header of a variable-length directory entry record. The `name` field
/// immediately follows this header in memory on disk.
///
/// `repr(C, packed)` is used to guarantee the exact 12-byte on-disk layout.
/// A plain `repr(C)` would pad the struct to 16 bytes due to the leading
/// `u64` field having alignment 8.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct EfsDirEntryHeader {
    /// Inode number of the referenced file. `0` means this slot is unused/deleted.
    pub inode: u64,
    /// Total length of this record in bytes (header + name + alignment padding).
    /// Used to skip to the next entry.
    pub rec_len: u16,
    /// Length of the filename in bytes. Maximum 255.
    pub name_len: u8,
    /// File type hint (see `FT_*` constants).
    pub file_type: u8,
}

const _: () = assert!(core::mem::size_of::<EfsDirEntryHeader>() == DIR_ENTRY_HEADER_SIZE);

/// Compute the minimum (and aligned) record length for a directory entry with
/// the given filename length. Records are always 4-byte aligned.
///
/// ```text
/// min_rec_len = (12 + name_len + 3) & !3
/// ```
pub fn dir_entry_min_size(name_len: u8) -> u16 {
    (DIR_ENTRY_HEADER_SIZE as u16 + name_len as u16 + 3) & !3
}
