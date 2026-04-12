use alloc::vec::Vec;

use crate::drivers::ahci::{AhciError, direct};

use super::block_page_cache::{BlockPageCache, BlockPageGuard};

const SECTOR_SIZE: usize = 512;
const SECTORS_PER_PAGE: u16 = 8;
const PAGE_SIZE: usize = 4096;

/// Device IDs >= this value are USB storage devices and route to the USB block API.
pub const USB_DEVICE_ID_BASE: u64 = 1000;

#[derive(Debug)]
pub struct BlockDevice {
    pub device_id: u64,
}

impl BlockDevice {
    /// Create a new BlockDevice. `_cache_size` is kept for call-site compatibility
    /// but is unused — caching is handled by the global BlockPageCache (lazy-init).
    pub fn new(device_id: u64, _cache_size: usize) -> Self {
        Self { device_id }
    }

    fn is_usb_device(&self) -> bool {
        self.device_id >= USB_DEVICE_ID_BASE
    }

    // ---- Page-level API (used by EFS) ------------------------------------

    /// Fetch a 4 KiB page from the block cache (fills from disk on miss).
    pub fn read_page(&self, page_block_idx: u64) -> Result<BlockPageGuard, AhciError> {
        BlockPageCache::global().read_page(self.device_id, page_block_idx)
    }

    /// Fetch multiple consecutive 4 KiB pages.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn write_partial_sectors(
        &self,
        lba: u64,
        sectors: u16,
        data: &[u8],
    ) -> Result<(), AhciError> {
        BlockPageCache::global().write_partial_page(self.device_id, lba, sectors, data)
    }

    /// Flush all dirty cached pages for this device then issue an AHCI cache flush.
    pub fn flush(&self) -> Result<(), AhciError> {
        BlockPageCache::global().flush_device(self.device_id)?;
        if !self.is_usb_device() {
            direct::flush_cache(self.device_id)?;
        }
        Ok(())
    }

    // ---- Legacy sector-level shims (kept for FAT32 compatibility) --------
    //
    // EFS no longer calls these — it uses read_page/write_page directly.
    // FAT32 still relies on them until it is migrated separately.

    /// Read `sectors` starting at `lba`. Uses the block page cache where
    /// alignment allows; falls back to partial-page reads otherwise.
    pub fn read_sectors(
        &self,
        lba: u64,
        sectors: u16,
        mut buffer: Vec<u8>,
    ) -> Result<Vec<u8>, AhciError> {
        let total_bytes = sectors as usize * SECTOR_SIZE;
        buffer.resize(total_bytes, 0);

        // Walk through covering pages, copying the relevant sectors out.
        let first_page = lba / SECTORS_PER_PAGE as u64;
        let last_lba = lba + sectors as u64 - 1;
        let last_page = last_lba / SECTORS_PER_PAGE as u64;

        let mut buf_pos = 0usize;
        for page_idx in first_page..=last_page {
            let guard = BlockPageCache::global().read_page(self.device_id, page_idx)?;
            let page_start_lba = page_idx * SECTORS_PER_PAGE as u64;

            // Which sectors of this page do we need?
            let sec_start = lba.max(page_start_lba) - page_start_lba;
            let sec_end_exclusive = (lba + sectors as u64)
                .min(page_start_lba + SECTORS_PER_PAGE as u64)
                - page_start_lba;

            let byte_start = sec_start as usize * SECTOR_SIZE;
            let byte_end = sec_end_exclusive as usize * SECTOR_SIZE;
            let slice = &guard.as_slice()[byte_start..byte_end];
            let len = slice.len();
            buffer[buf_pos..buf_pos + len].copy_from_slice(slice);
            buf_pos += len;
        }

        Ok(buffer)
    }

    /// Write `sectors` starting at `lba`. Routes through the page cache.
    pub fn write_sectors(&self, lba: u64, data: &[u8], sectors: u16) -> Result<(), AhciError> {
        assert_eq!(data.len(), sectors as usize * SECTOR_SIZE);

        let first_page = lba / SECTORS_PER_PAGE as u64;
        let last_lba = lba + sectors as u64 - 1;
        let last_page = last_lba / SECTORS_PER_PAGE as u64;

        let mut data_pos = 0usize;
        for page_idx in first_page..=last_page {
            let page_start_lba = page_idx * SECTORS_PER_PAGE as u64;

            let sec_start = lba.max(page_start_lba) - page_start_lba;
            let sec_end_exclusive = (lba + sectors as u64)
                .min(page_start_lba + SECTORS_PER_PAGE as u64)
                - page_start_lba;

            let byte_count = (sec_end_exclusive - sec_start) as usize * SECTOR_SIZE;
            let write_lba = page_start_lba + sec_start;
            let write_sectors = (sec_end_exclusive - sec_start) as u16;

            if sec_start == 0 && sec_end_exclusive == SECTORS_PER_PAGE as u64 {
                // Aligned full-page write.
                let mut buf = [0u8; PAGE_SIZE];
                buf.copy_from_slice(&data[data_pos..data_pos + PAGE_SIZE]);
                BlockPageCache::global().write_page(self.device_id, page_idx, &buf)?;
            } else {
                // Partial-page write (RMW).
                BlockPageCache::global().write_partial_page(
                    self.device_id,
                    write_lba,
                    write_sectors,
                    &data[data_pos..data_pos + byte_count],
                )?;
            }
            data_pos += byte_count;
        }

        Ok(())
    }
}
