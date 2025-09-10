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

#[derive(Debug)]
pub struct BlockDevice {
    pub device_id: u64,
    pub cache: Mutex<LruCache<u64, [u8; SECTOR_SIZE]>>,
}

impl BlockDevice {
    pub fn new(device_id: u64, cache_size: usize) -> Self {
        Self {
            device_id,
            cache: Mutex::new(LruCache::new(NonZero::new(cache_size).unwrap())),
        }
    }

    pub fn read_sectors(
        &self,
        lba: u64,
        sectors: u16,
        mut buffer: Vec<u8>,
    ) -> Result<Vec<u8>, AhciError> {
        buffer.resize(sectors as usize * SECTOR_SIZE, 0);

        let mut cache = self.cache.lock();
        let mut miss_ranges = Vec::new();

        // Step 1: fill from cache
        for i in 0..sectors as u64 {
            let idx = (i as usize) * SECTOR_SIZE;
            let lba_i = lba + i;
            if let Some(sector) = cache.get(&lba_i) {
                buffer[idx..idx + SECTOR_SIZE].copy_from_slice(sector);
            } else {
                miss_ranges.push((lba_i, idx));
            }
        }

        // Step 2: fetch misses in one backend call if contiguous
        if !miss_ranges.is_empty() {
            let first = miss_ranges[0].0;
            let count = miss_ranges.len() as u16;
            let mut tmp = vec![0u8; count as usize * SECTOR_SIZE];
            let mut data = read_sectors(self.device_id, first, count, tmp)?;

            for (j, (lba_i, idx)) in miss_ranges.iter().enumerate() {
                let start = j * SECTOR_SIZE;
                let end = start + SECTOR_SIZE;
                buffer[*idx..*idx + SECTOR_SIZE].copy_from_slice(&data[start..end]);

                let mut sector = [0u8; SECTOR_SIZE];
                sector.copy_from_slice(&data[start..end]);
                cache.put(*lba_i, sector);
            }
        }

        Ok(buffer)
    }

    pub fn write_sectors(
        &self,
        lba: u64,
        mut data: Vec<u8>,
        sectors: u16,
    ) -> Result<Vec<u8>, AhciError> {
        assert_eq!(data.len(), sectors as usize * SECTOR_SIZE);
        {
            let mut cache = self.cache.lock();
            for i in 0..sectors as u64 {
                let idx = (i as usize) * SECTOR_SIZE;
                let mut sector = [0u8; SECTOR_SIZE];
                sector.copy_from_slice(&data[idx..idx + SECTOR_SIZE]);
                cache.put(lba + i, sector);
            }
        }
        // write-through
        write_sectors(self.device_id, lba, data, sectors)
    }

    pub fn flush(&self) -> Result<(), AhciError> {
        flush_cache(self.device_id)
    }
}
