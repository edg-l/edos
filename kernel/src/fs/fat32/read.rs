use alloc::vec::Vec;

use crate::fs::{
    Error,
    fat32::{Fat32fs, structures::DirectoryEntry},
};

impl Fat32fs {
    pub fn read_file(&self, entry: &DirectoryEntry) -> Result<Vec<u8>, Error> {
        if entry.is_directory() {
            return Err(Error::NotAFile);
        }

        let file_size = entry.file_size as usize;
        if file_size == 0 {
            return Ok(Vec::new());
        }

        let spc = self.boot_info.sectors_per_cluster as u16;
        let cluster_bytes = (self.boot_info.bytes_per_sector as usize) * (spc as usize);

        let mut cluster = entry.first_cluster();
        if cluster < 2 {
            return Err(Error::Corrupted); // size>0 but no valid first cluster
        }

        let mut out = Vec::with_capacity(file_size);
        let mut remaining = file_size;

        let mut buf = Vec::new();
        loop {
            // read one cluster
            let base_lba = self.cluster_to_lba(cluster);
            buf.clear();
            buf = self.device.read_sectors(base_lba, spc, buf)?; // len >= cluster_bytes

            // copy only what we need from this cluster
            let take = remaining.min(buf.len().min(cluster_bytes));
            out.extend_from_slice(&buf[..take]);
            remaining -= take;

            if remaining == 0 {
                break;
            }

            // advance FAT
            match self.get_fat_entry(cluster)? {
                Some(next) => cluster = next,
                None => {
                    // chain ended before file_size bytes read → inconsistency
                    return Err(Error::Corrupted);
                }
            }
        }

        Ok(out)
    }

    pub fn read_file_offset(
        &self,
        entry: &DirectoryEntry,
        offset: usize,
        count: usize,
    ) -> Result<Vec<u8>, Error> {
        if entry.is_directory() {
            return Err(Error::NotAFile);
        }

        let file_size = entry.file_size as usize;
        if offset >= file_size {
            return Ok(Vec::new());
        }

        let to_read = count.min(file_size - offset);
        let mut out = Vec::with_capacity(to_read);

        let spc = self.boot_info.sectors_per_cluster as usize;
        let cluster_size = spc * self.boot_info.bytes_per_sector as usize;

        // Find starting cluster for offset
        let mut cluster = entry.first_cluster();
        let mut cluster_index = 0;
        while offset >= (cluster_index + 1) * cluster_size {
            cluster_index += 1;
            match self.get_fat_entry(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(Vec::new()), // end of chain before offset
            }
        }

        // Offset inside this cluster
        let mut inner_off = offset - cluster_index * cluster_size;
        let mut remain = to_read;

        let mut buf = Vec::new();
        loop {
            let base_lba = self.cluster_to_lba(cluster);

            buf.clear();
            buf = self.device.read_sectors(base_lba, spc as u16, buf)?;

            // slice cluster content
            let cluster_bytes = &buf[inner_off..cluster_size.min(buf.len())];
            let take = remain.min(cluster_bytes.len());
            out.extend_from_slice(&cluster_bytes[..take]);

            remain -= take;
            if remain == 0 {
                break;
            }

            // advance to next cluster
            match self.get_fat_entry(cluster)? {
                Some(next) => {
                    cluster = next;
                    inner_off = 0;
                }
                None => break, // end of chain
            }
        }

        Ok(out)
    }
}
