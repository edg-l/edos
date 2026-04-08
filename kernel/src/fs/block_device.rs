use core::num::NonZero;

use alloc::vec;
use alloc::vec::Vec;
use lru::LruCache;
use spin::Mutex;

use crate::drivers::ahci::{
    AhciError,
    api::{flush_cache, read_sectors, write_sectors},
};

const SECTOR_SIZE: usize = 512;

/// Device IDs >= this value are USB storage devices and route to the USB block API.
const USB_DEVICE_ID_BASE: u64 = 1000;

#[derive(Debug)]
pub struct BlockDevice {
    pub device_id: u64,
    pub cache: LruCache<u64, [u8; SECTOR_SIZE]>,
    scratch: Vec<u8>,
}

impl BlockDevice {
    pub fn new(device_id: u64, cache_size: usize) -> Self {
        Self {
            device_id,
            cache: LruCache::new(NonZero::new(cache_size).unwrap()),
            scratch: Vec::new(),
        }
    }

    fn is_usb_device(&self) -> bool {
        self.device_id >= USB_DEVICE_ID_BASE
    }

    pub fn read_sectors(
        &mut self,
        lba: u64,
        sectors: u16,
        mut buffer: Vec<u8>,
    ) -> Result<Vec<u8>, AhciError> {
        buffer.resize(sectors as usize * SECTOR_SIZE, 0);

        let mut miss_ranges = Vec::new();

        for i in 0..sectors as u64 {
            let idx = (i as usize) * SECTOR_SIZE;
            let lba_i = lba + i;
            if let Some(sector) = self.cache.get(&lba_i) {
                buffer[idx..idx + SECTOR_SIZE].copy_from_slice(sector);
            } else {
                miss_ranges.push((lba_i, idx));
            }
        }

        if !miss_ranges.is_empty() {
            // Group misses into contiguous sub-ranges to avoid reading wrong sectors
            let mut i = 0;
            while i < miss_ranges.len() {
                let run_start = i;
                let first_lba = miss_ranges[run_start].0;
                // Extend run while LBAs are consecutive
                while i + 1 < miss_ranges.len() && miss_ranges[i + 1].0 == miss_ranges[i].0 + 1 {
                    i += 1;
                }
                let run_count = (i - run_start + 1) as u16;

                self.scratch.resize(run_count as usize * SECTOR_SIZE, 0);
                let scratch = core::mem::take(&mut self.scratch);

                let data = if self.is_usb_device() {
                    crate::drivers::usb::block_api::usb_read_sectors(first_lba, run_count, scratch)
                        .map_err(|_| AhciError::IoError)?
                } else {
                    read_sectors(self.device_id, first_lba, run_count, scratch)?
                };

                for (j, (lba_j, idx)) in miss_ranges[run_start..=i].iter().enumerate() {
                    let start = j * SECTOR_SIZE;
                    buffer[*idx..*idx + SECTOR_SIZE]
                        .copy_from_slice(&data[start..start + SECTOR_SIZE]);

                    let mut sector = [0u8; SECTOR_SIZE];
                    sector.copy_from_slice(&data[start..start + SECTOR_SIZE]);
                    self.cache.put(*lba_j, sector);
                }

                self.scratch = data;
                i += 1;
            }
        }

        Ok(buffer)
    }

    pub fn write_sectors(
        &mut self,
        lba: u64,
        mut data: Vec<u8>,
        sectors: u16,
    ) -> Result<Vec<u8>, AhciError> {
        assert_eq!(data.len(), sectors as usize * SECTOR_SIZE);

        {
            for i in 0..sectors as u64 {
                let idx = (i as usize) * SECTOR_SIZE;
                let mut sector = [0u8; SECTOR_SIZE];
                sector.copy_from_slice(&data[idx..idx + SECTOR_SIZE]);
                self.cache.put(lba + i, sector);
            }
        }

        let mut tmp = core::mem::take(&mut self.scratch);
        core::mem::swap(&mut tmp, &mut data);

        let out = if self.is_usb_device() {
            crate::drivers::usb::block_api::usb_write_sectors(lba, sectors, tmp)
                .map_err(|_| AhciError::IoError)?
        } else {
            write_sectors(self.device_id, lba, tmp, sectors)?
        };

        self.scratch = out; // reclaim backend buffer
        Ok(data) // caller gets their Vec back
    }

    pub fn flush(&self) -> Result<(), AhciError> {
        if self.is_usb_device() {
            // USB mass storage has no flush command; treat as a no-op.
            return Ok(());
        }
        flush_cache(self.device_id)
    }
}
