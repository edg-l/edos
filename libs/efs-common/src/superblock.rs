/// Primary superblock structure. Stored at block 1, first 256 bytes.
/// Bytes 256 through end-of-block must be zero on disk.
///
/// `repr(C, packed)` is used to guarantee the exact on-disk byte layout with
/// no compiler-inserted padding. The spec defines offsets 176-187 as
/// `mount_count(2) + max_mount_count(2) + first_data_block(8)` with no
/// padding between them; a plain `repr(C)` would insert 4 bytes before the
/// `u64` field.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct EfsSuperblock {
    /// Magic number: `0x45465321` (ASCII "EFS!")
    pub magic: u32,
    /// On-disk format version. Must be 1 for this spec.
    pub version: u32,
    /// Total number of blocks in the filesystem.
    pub total_blocks: u64,
    /// Total number of inodes allocated across all groups.
    pub total_inodes: u64,
    /// Current count of free (unallocated) blocks.
    pub free_blocks: u64,
    /// Current count of free (unallocated) inodes.
    pub free_inodes: u64,
    /// log2(block_size_in_bytes). 12 = 4096 bytes, 13 = 8192 bytes.
    pub block_size_log2: u32,
    /// Number of blocks per block group.
    pub blocks_per_group: u32,
    /// Number of inodes per block group.
    pub inodes_per_group: u32,
    /// Size of each inode in bytes. Must be 256 for v1.
    pub inode_size: u16,
    /// Total number of block groups.
    pub block_group_count: u16,
    /// Compatible feature flag bitmask.
    pub compatible_features: u64,
    /// Incompatible feature flag bitmask.
    pub incompatible_features: u64,
    /// Read-only compatible feature flag bitmask.
    pub read_only_features: u64,
    /// Filesystem UUID (RFC 4122 format recommended).
    pub uuid: [u8; 16],
    /// Null-terminated UTF-8 volume label. Unused bytes must be zero.
    pub volume_name: [u8; 64],
    /// Unix timestamp (seconds since epoch) of the last mount.
    pub mount_time: u64,
    /// Unix timestamp (seconds since epoch) of the last write.
    pub write_time: u64,
    /// Number of times mounted since last fsck.
    pub mount_count: u16,
    /// Recommended maximum mount count before fsck. 0 = no limit.
    pub max_mount_count: u16,
    /// Block number of the first block not used by superblock or BGD table.
    pub first_data_block: u64,
    /// CRC32 of the first 256 bytes of the superblock with this field zeroed.
    pub checksum: u32,
    /// Reserved; must be zero.
    pub reserved: [u8; 64],
}

const _: () = assert!(core::mem::size_of::<EfsSuperblock>() == 256);

impl EfsSuperblock {
    /// Returns the block size in bytes.
    pub fn block_size(&self) -> u32 {
        1u32 << self.block_size_log2
    }

    /// Validate the superblock fields for basic correctness.
    ///
    /// Returns `Ok(())` if valid. On checksum mismatch the spec says to warn
    /// and continue, so this only returns `Err` for hard structural failures.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.magic != crate::EFS_MAGIC {
            return Err("bad magic");
        }
        if self.version != crate::EFS_VERSION {
            return Err("unsupported version");
        }
        if self.inode_size != 256 {
            return Err("inode_size must be 256");
        }
        if !matches!(self.block_size_log2, 10..=13) {
            return Err("block_size_log2 must be 10, 11, 12, or 13");
        }
        Ok(())
    }
}

/// Returns `true` if block group `group` should contain a superblock and BGD
/// table backup.
///
/// Backups are stored in groups 0, 1, and groups whose number is a positive
/// power of 3, 5, or 7.
pub fn has_superblock_backup(group: u16) -> bool {
    if group <= 1 {
        return true;
    }
    is_power_of(group as u64, 3) || is_power_of(group as u64, 5) || is_power_of(group as u64, 7)
}

fn is_power_of(mut n: u64, base: u64) -> bool {
    while n > 1 {
        if !n.is_multiple_of(base) {
            return false;
        }
        n /= base;
    }
    n == 1
}
