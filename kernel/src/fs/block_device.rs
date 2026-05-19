use alloc::vec::Vec;

use crate::drivers::ahci::AhciError;

use super::block_page_cache::{BlockPageCache, BlockPageGuard};

const PAGE_SIZE: usize = 4096;

#[derive(Debug)]
pub struct BlockDevice {
    pub device_id: u64,
}

impl BlockDevice {
    /// Create a new BlockDevice. Caching is handled by the global `BlockPageCache`.
    pub fn new(device_id: u64) -> Self {
        Self { device_id }
    }

    // ---- Page-level API (used by EFS) ------------------------------------

    /// Fetch a 4 KiB page from the block cache (fills from disk on miss).
    pub fn read_page(&self, page_block_idx: u64) -> Result<BlockPageGuard, AhciError> {
        BlockPageCache::global().read_page(self.device_id, page_block_idx)
    }

    /// Fetch multiple consecutive 4 KiB pages.
    pub fn read_pages(
        &self,
        start_page: u64,
        count: usize,
    ) -> Result<Vec<BlockPageGuard>, AhciError> {
        BlockPageCache::global().read_pages(self.device_id, start_page, count)
    }

    /// Write a full 4 KiB page (write-through).
    pub fn write_page(&self, page_block_idx: u64, data: &[u8; PAGE_SIZE]) -> Result<(), AhciError> {
        BlockPageCache::global().write_page(self.device_id, page_block_idx, data)
    }

    /// Write a sub-page sector range (RMW, write-through).
    pub fn write_partial_sectors(
        &self,
        lba: u64,
        sectors: u16,
        data: &[u8],
    ) -> Result<(), AhciError> {
        BlockPageCache::global().write_partial_page(self.device_id, lba, sectors, data)
    }

    /// Flush all dirty cached pages for this device, then issue the hardware
    /// cache flush via the block-io trait. `submit_flush` is a no-op on
    /// devices without a write cache.
    pub fn flush(&self) -> Result<(), AhciError> {
        BlockPageCache::global().flush_device(self.device_id)
    }
}
