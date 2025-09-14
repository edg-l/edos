use alloc::vec::Vec;

use crate::fs::{
    Error,
    fat32::{Fat32fs, structures::DirectoryEntry},
};

impl Fat32fs {
    /// Maximum sectors to read in a single batched operation (8MB with 512-byte sectors).
    const MAX_BATCH_SECTORS: u16 = 4096;

    /// Read consecutive clusters in a single batched operation.
    /// Returns the data read from the clusters.
    fn read_consecutive_clusters(
        &self,
        start_cluster: u32,
        cluster_count: u32,
        mut buffer: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        let spc = self.boot_info.sectors_per_cluster as u16;
        let total_sectors = (cluster_count as u16).saturating_mul(spc);

        // If the batch is too large, split it into smaller reads
        if total_sectors > Self::MAX_BATCH_SECTORS {
            let clusters_per_batch = (Self::MAX_BATCH_SECTORS / spc) as u32;
            let mut remaining_clusters = cluster_count;
            let mut current_cluster = start_cluster;

            buffer.clear();
            let cluster_bytes = (spc as usize) * (self.boot_info.bytes_per_sector as usize);

            while remaining_clusters > 0 {
                let batch_size = remaining_clusters.min(clusters_per_batch);
                let batch_sectors = (batch_size as u16) * spc;

                let base_lba = self.cluster_to_lba(current_cluster);
                let batch_data = self
                    .device
                    .read_sectors(base_lba, batch_sectors, Vec::new())?;

                // Only take the bytes we need from this batch
                let bytes_to_take = (batch_size as usize) * cluster_bytes;
                buffer.extend_from_slice(&batch_data[..bytes_to_take.min(batch_data.len())]);

                current_cluster += batch_size;
                remaining_clusters -= batch_size;
            }

            Ok(buffer)
        } else {
            // Read all clusters in one operation
            let base_lba = self.cluster_to_lba(start_cluster);
            buffer = self.device.read_sectors(base_lba, total_sectors, buffer)?;
            Ok(buffer)
        }
    }
    pub fn read_file(&self, entry: &DirectoryEntry) -> Result<Vec<u8>, Error> {
        if entry.is_directory() {
            return Err(Error::NotAFile);
        }

        let file_size = entry.file_size as usize;
        if file_size == 0 {
            return Ok(Vec::new());
        }

        let start_cluster = entry.first_cluster();
        if start_cluster < 2 {
            return Err(Error::Corrupted); // size>0 but no valid first cluster
        }

        let cluster_bytes = (self.boot_info.bytes_per_sector as usize)
            * (self.boot_info.sectors_per_cluster as usize);

        // Analyze the cluster chain to find consecutive ranges
        let cluster_ranges = self.analyze_cluster_chain(start_cluster)?;

        let mut out = Vec::with_capacity(file_size);
        let mut remaining = file_size;

        // Process each consecutive range
        for (range_start, range_count) in cluster_ranges {
            if remaining == 0 {
                break;
            }

            // Read the entire consecutive range in one operation
            let range_data =
                self.read_consecutive_clusters(range_start, range_count, Vec::new())?;

            // Take only what we need from this range
            let range_bytes = (range_count as usize) * cluster_bytes;
            let take = remaining.min(range_data.len().min(range_bytes));
            out.extend_from_slice(&range_data[..take]);
            remaining -= take;
        }

        // Verify we read the expected amount
        if remaining > 0 {
            return Err(Error::Corrupted); // chain ended before file_size bytes read
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
        let cluster_size = (self.boot_info.sectors_per_cluster as usize)
            * (self.boot_info.bytes_per_sector as usize);

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

        // Offset inside the starting cluster
        let inner_offset = offset - cluster_index * cluster_size;

        // Calculate how many clusters we need to read
        let end_offset = offset + to_read;
        let start_cluster_index = cluster_index;
        let end_cluster_index = (end_offset - 1) / cluster_size; // 0-based
        let clusters_needed = (end_cluster_index - start_cluster_index + 1) as u32;

        // Analyze cluster chain starting from our starting cluster to find consecutive ranges
        let cluster_ranges = self.analyze_cluster_chain(cluster)?;

        let mut out = Vec::with_capacity(to_read);
        let mut remaining = to_read;
        let mut clusters_processed = 0u32;
        let mut is_first_range = true;

        // Process each consecutive range
        for (range_start, range_count) in cluster_ranges {
            if remaining == 0 || clusters_processed >= clusters_needed {
                break;
            }

            // Adjust range if we don't need all clusters
            let clusters_to_process = range_count.min(clusters_needed - clusters_processed);

            // Read the consecutive clusters
            let range_data =
                self.read_consecutive_clusters(range_start, clusters_to_process, Vec::new())?;

            // Handle the data extraction with proper offset handling
            let mut data_start_offset = 0;
            if is_first_range {
                data_start_offset = inner_offset;
                is_first_range = false;
            }

            let data_end = range_data.len();
            let available_bytes = data_end.saturating_sub(data_start_offset);
            let take = remaining.min(available_bytes);

            if take > 0 && data_start_offset < data_end {
                out.extend_from_slice(&range_data[data_start_offset..data_start_offset + take]);
                remaining -= take;
            }

            clusters_processed += clusters_to_process;
        }

        Ok(out)
    }
}
