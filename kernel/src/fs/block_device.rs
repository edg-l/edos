use lru::LruCache;
use spin::Mutex;

use crate::drivers::ahci::api::read_sectors;

#[derive(Debug)]
pub struct BlockDevice {
    pub device_id: u64,
    pub cache: Mutex<LruCache<u64, [u8; 512]>>,
}

impl BlockDevice {
    pub fn read_sectors(&self, lba: u64, sectors: u16) {
        let sectors = read_sectors(self.device_id, lba, sectors);
    }
}
